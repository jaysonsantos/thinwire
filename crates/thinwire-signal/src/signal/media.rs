// SPDX-License-Identifier: AGPL-3.0-only
//! Images and audio of Signal messages.
//!
//! A photo downloads when its chat is open, and its row comes again with
//! the image. The image crosses the pipe as bytes, so a large photo is made
//! smaller first. A voice note or an audio file downloads when the user
//! presses Play. `Debug` shows no key, no path, and no bytes.

use std::collections::{HashMap, VecDeque};
use std::fmt;

use presage::proto::AttachmentPointer;
use thinwire_protocol::{ChatMessage, MessageAudio, MessagePhoto};

/// The largest photo the helper downloads.
const PHOTO_DOWNLOAD_MAX: u32 = 25 * 1024 * 1024;

/// Photos that the helper keeps in memory. Past this, the oldest goes. A
/// chat that opens again downloads a dropped photo again.
const PHOTO_CACHE: usize = 200;

/// The flag of a voice note in [`AttachmentPointer::flags`].
const VOICE_MESSAGE_FLAG: u32 = 1;

/// What an attachment is, for its label and its handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Photo,
    Voice,
    Audio,
    File,
}

pub(super) fn kind(pointer: &AttachmentPointer) -> Kind {
    let content = pointer.content_type.as_deref().unwrap_or("");
    if content.starts_with("image/") {
        Kind::Photo
    } else if content.starts_with("audio/") {
        if pointer.flags.unwrap_or(0) & VOICE_MESSAGE_FLAG != 0 {
            Kind::Voice
        } else {
            Kind::Audio
        }
    } else {
        Kind::File
    }
}

/// The body of a message with no text, from its first attachment.
pub(super) const fn label(kind: Kind) -> &'static str {
    match kind {
        Kind::Photo => "[photo]",
        Kind::Voice => "[voice message]",
        Kind::Audio => "[audio]",
        Kind::File => "[file]",
    }
}

/// The handle of the audio of one message for `DownloadVoice`. The same
/// message has the same handle in every run. Always positive.
pub(super) fn voice_file_id(message_id: &str) -> i32 {
    use std::hash::{BuildHasher, BuildHasherDefault, DefaultHasher};
    let hash = BuildHasherDefault::<DefaultHasher>::default().hash_one(message_id);
    let low = u32::try_from(hash & 0x7fff_ffff).unwrap_or(1);
    i32::try_from(low).unwrap_or(1).max(1)
}

/// One photo to download. The row of the message waits for it.
#[derive(Clone)]
pub(super) struct PhotoJob {
    pub message: ChatMessage,
    pub pointer: AttachmentPointer,
    /// The text of the message. The row shows it under the image.
    pub caption: String,
}

impl fmt::Debug for PhotoJob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PhotoJob(<redacted>)")
    }
}

#[derive(Clone)]
enum PhotoState {
    Requested,
    Ready(MessagePhoto),
    Failed,
}

#[derive(Default)]
struct Book {
    voices: HashMap<i32, AttachmentPointer>,
    photos: HashMap<String, PhotoState>,
    order: VecDeque<String>,
    /// Photo downloads for each chat. They start when the chat is open.
    waiting: HashMap<String, Vec<PhotoJob>>,
}

/// The photos and the audio handles of the live account.
#[derive(Default)]
pub(super) struct MediaBook {
    book: std::sync::Mutex<Book>,
}

impl fmt::Debug for MediaBook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MediaBook")
    }
}

impl MediaBook {
    fn lock(&self) -> std::sync::MutexGuard<'_, Book> {
        // A poisoned lock still holds the handles. A panic must not drop them.
        self.book
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Set the photo and the audio of `message` from its attachments.
    ///
    /// A photo in the cache shows at once, with `caption` as the body. A new
    /// photo waits in the queue of its chat, and the row keeps its label.
    pub(super) fn dress(
        &self,
        message: &mut ChatMessage,
        attachments: &[AttachmentPointer],
        caption: Option<&str>,
    ) {
        let mut book = self.lock();
        if let Some(pointer) = attachments
            .iter()
            .find(|pointer| kind(pointer) == Kind::Photo)
        {
            match book.photos.get(&message.id).cloned() {
                Some(PhotoState::Ready(photo)) => {
                    message.photo = Some(photo);
                    message.body = caption.unwrap_or("").to_string();
                }
                Some(PhotoState::Requested | PhotoState::Failed) => {}
                None if pointer.size.is_some_and(|size| size > PHOTO_DOWNLOAD_MAX) => {}
                None => {
                    book.photos
                        .insert(message.id.clone(), PhotoState::Requested);
                    book.order.push_back(message.id.clone());
                    trim(&mut book);
                    book.waiting
                        .entry(message.conversation_id.clone())
                        .or_default()
                        .push(PhotoJob {
                            message: message.clone(),
                            pointer: pointer.clone(),
                            caption: caption.unwrap_or("").to_string(),
                        });
                }
            }
        }
        if let Some(pointer) = attachments
            .iter()
            .find(|pointer| matches!(kind(pointer), Kind::Voice | Kind::Audio))
        {
            let file_id = voice_file_id(&message.id);
            book.voices.insert(file_id, pointer.clone());
            message.audio = MessageAudio::new(0, file_id, None);
        }
    }

    /// The photo downloads of `conversation_id`. Each one is returned once.
    pub(super) fn take_jobs(&self, conversation_id: &str) -> Vec<PhotoJob> {
        self.lock()
            .waiting
            .remove(conversation_id)
            .unwrap_or_default()
    }

    /// A photo download ended. With an image, the row of the message comes
    /// back with it. A failed download keeps the label and does not ask
    /// again.
    pub(super) fn finish(&self, job: PhotoJob, photo: Option<MessagePhoto>) -> Option<ChatMessage> {
        let mut book = self.lock();
        if !matches!(
            book.photos.get(&job.message.id),
            Some(PhotoState::Requested)
        ) {
            // The cache dropped the request. The row asks again later.
            return None;
        }
        let Some(photo) = photo else {
            book.photos.insert(job.message.id, PhotoState::Failed);
            return None;
        };
        book.photos
            .insert(job.message.id.clone(), PhotoState::Ready(photo.clone()));
        let mut message = job.message;
        message.photo = Some(photo);
        message.body = job.caption;
        Some(message)
    }

    /// The attachment of the audio `file_id`, when a row of it was shown.
    pub(super) fn voice(&self, file_id: i32) -> Option<AttachmentPointer> {
        self.lock().voices.get(&file_id).cloned()
    }
}

fn trim(book: &mut Book) {
    while book.order.len() > PHOTO_CACHE {
        if let Some(old) = book.order.pop_front() {
            book.photos.remove(&old);
        }
    }
}

/// The photo that crosses the pipe, from the downloaded file.
pub(super) fn photo_from_bytes(bytes: &[u8]) -> Option<MessagePhoto> {
    thinwire_protocol::photo_resize::photo_from_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use thinwire_protocol::{Arrival, Delivery, ProtocolId};

    fn pointer(content_type: &str, flags: u32) -> AttachmentPointer {
        AttachmentPointer {
            content_type: Some(content_type.into()),
            flags: Some(flags),
            size: Some(1000),
            ..Default::default()
        }
    }

    fn message(id: &str) -> ChatMessage {
        ChatMessage {
            protocol: ProtocolId::Signal,
            conversation_id: "signal:chat".into(),
            id: id.into(),
            sender: "Ana".into(),
            body: "[photo]".into(),
            outbound: false,
            delivery: Delivery::Sent,
            sent_at: 1,
            arrival: Arrival::Live,
            topic: None,
            links: Vec::new(),
            photo: None,
            audio: None,
            video: None,
            file: None,
            reactions: Vec::new(),
            reply_to: None,
        }
    }

    #[test]
    fn an_attachment_has_a_kind_and_a_label() {
        assert_eq!(kind(&pointer("image/jpeg", 0)), Kind::Photo);
        assert_eq!(kind(&pointer("audio/aac", 1)), Kind::Voice);
        assert_eq!(kind(&pointer("audio/mpeg", 0)), Kind::Audio);
        assert_eq!(kind(&pointer("application/pdf", 0)), Kind::File);
        assert_eq!(label(Kind::Voice), "[voice message]");
        assert_eq!(voice_file_id("m1"), voice_file_id("m1"));
        assert!(voice_file_id("m1") > 0);
    }

    /// A new photo waits in the queue of its chat. Its row comes back with
    /// the image and the caption. The next row of it shows the image at once.
    #[test]
    fn a_photo_waits_for_its_chat_and_then_shows() {
        let book = MediaBook::default();
        let mut row = message("m1");
        book.dress(&mut row, &[pointer("image/png", 0)], Some("look"));
        assert!(row.photo.is_none());
        assert_eq!(row.body, "[photo]");
        assert!(book.take_jobs("signal:other").is_empty());
        let jobs = book.take_jobs("signal:chat");
        assert_eq!(jobs.len(), 1);
        assert!(book.take_jobs("signal:chat").is_empty(), "each job once");
        let photo = thinwire_protocol::photo_resize::photo(4, 2, vec![1, 2, 3]);
        let shown = book
            .finish(jobs[0].clone(), Some(photo))
            .expect("the row comes back");
        assert!(shown.photo.is_some());
        assert_eq!(shown.body, "look");
        let mut again = message("m1");
        book.dress(&mut again, &[pointer("image/png", 0)], None);
        assert!(again.photo.is_some());
        assert_eq!(again.body, "");
        assert!(book.take_jobs("signal:chat").is_empty(), "no new download");
    }

    /// A failed photo keeps the label and does not ask again.
    #[test]
    fn a_failed_photo_keeps_the_label() {
        let book = MediaBook::default();
        let mut row = message("m2");
        book.dress(&mut row, &[pointer("image/jpeg", 0)], None);
        let job = book.take_jobs("signal:chat").remove(0);
        assert!(book.finish(job, None).is_none());
        let mut again = message("m2");
        book.dress(&mut again, &[pointer("image/jpeg", 0)], None);
        assert!(again.photo.is_none());
        assert!(book.take_jobs("signal:chat").is_empty());
    }

    /// A voice note gets a handle. The handle finds its attachment.
    #[test]
    fn a_voice_note_has_a_handle_for_its_attachment() {
        let book = MediaBook::default();
        let mut row = message("m3");
        row.body = "[voice message]".into();
        book.dress(&mut row, &[pointer("audio/aac", 1)], None);
        let audio = row.audio.expect("audio");
        assert_eq!(audio.file_id(), voice_file_id("m3"));
        assert_eq!(audio.duration_secs(), 0);
        assert!(book.voice(audio.file_id()).is_some());
        assert!(book.voice(audio.file_id().wrapping_add(1).max(1)).is_none());
    }
}
