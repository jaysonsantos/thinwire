// SPDX-License-Identifier: AGPL-3.0-only
//! Images and audio of Signal messages.
//!
//! A photo downloads when its chat is open, and its row comes again with
//! the image. The image crosses the pipe as bytes, so a large photo is made
//! smaller first. A voice note or an audio file downloads when the user
//! presses Play. `Debug` shows no key, no path, and no bytes.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io::Cursor;

use presage::proto::AttachmentPointer;
use thinwire_protocol::{ChatMessage, ImageBytes, MessageAudio, MessagePhoto, PhotoSize};

/// The largest image that crosses the pipe. It is
/// `thinwire_ipc::WirePhoto::MAX_BYTES` (a test checks it).
pub(super) const PHOTO_MAX_BYTES: usize = 512 * 1024;

/// The largest photo the helper downloads.
const PHOTO_DOWNLOAD_MAX: u32 = 25 * 1024 * 1024;

/// The longest side of a photo after it is made smaller, in pixels. The
/// second value is for a photo that is still too large after the first.
const PHOTO_SIDES: [u32; 2] = [1280, 800];

/// The JPEG quality for each side of [`PHOTO_SIDES`].
const PHOTO_QUALITY: [u8; 2] = [82, 70];

/// Photos that the helper keeps in memory. Past this, the oldest goes. A
/// chat that opens again downloads a dropped photo again.
const PHOTO_CACHE: usize = 200;

/// The largest image that the decoder accepts, in pixels for each side.
const DECODE_MAX_SIDE: u32 = 12_000;

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

/// The photo that crosses the pipe, from the downloaded file. A JPEG, PNG,
/// or WebP file that fits stays as it is. Any other image is made smaller
/// and becomes a JPEG. `None` when the bytes are not an image, or when the
/// image does not fit at the smallest size.
pub(super) fn photo_from_bytes(bytes: &[u8]) -> Option<MessagePhoto> {
    let reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let format = reader.format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(DECODE_MAX_SIDE);
    limits.max_image_height = Some(DECODE_MAX_SIDE);
    let keep = bytes.len() <= PHOTO_MAX_BYTES
        && matches!(
            format,
            image::ImageFormat::Jpeg | image::ImageFormat::Png | image::ImageFormat::WebP
        );
    if keep {
        let mut sized = image::ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .ok()?;
        sized.limits(limits);
        let (width, height) = sized.into_dimensions().ok()?;
        return Some(photo(width, height, bytes.to_vec()));
    }
    let mut decoder = reader;
    decoder.limits(limits);
    let image = decoder.decode().ok()?;
    for (side, quality) in PHOTO_SIDES.into_iter().zip(PHOTO_QUALITY) {
        let small = if image.width().max(image.height()) > side {
            image.resize(side, side, image::imageops::FilterType::Triangle)
        } else {
            image.clone()
        };
        let rgb = small.to_rgb8();
        let mut out = Vec::new();
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
        if rgb.write_with_encoder(encoder).is_err() {
            return None;
        }
        if out.len() <= PHOTO_MAX_BYTES {
            return Some(photo(rgb.width(), rgb.height(), out));
        }
    }
    None
}

fn photo(width: u32, height: u32, bytes: Vec<u8>) -> MessagePhoto {
    MessagePhoto {
        width,
        height,
        mini: None,
        sizes: vec![PhotoSize {
            width,
            height,
            file_id: None,
            path: None,
            bytes: Some(ImageBytes::new(bytes)),
        }],
        has_spoiler: false,
        is_secret: false,
        failed: false,
    }
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
        }
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbImage::from_pixel(width, height, image::Rgb([200, 40, 40]));
        let mut out = Cursor::new(Vec::new());
        image
            .write_to(&mut out, image::ImageFormat::Png)
            .expect("png");
        out.into_inner()
    }

    #[test]
    fn the_wire_limit_is_the_limit_of_the_pipe() {
        assert_eq!(PHOTO_MAX_BYTES, thinwire_ipc::WirePhoto::MAX_BYTES);
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
        let photo = photo_from_bytes(&png(4, 2)).expect("an image");
        assert_eq!((photo.width, photo.height), (4, 2));
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

    /// A large photo becomes a JPEG that fits the pipe, with its long side
    /// at most the first size.
    #[test]
    fn a_large_photo_is_made_smaller() {
        // Noise does not compress, so this PNG is larger than the pipe.
        let mut seed = 7_u32;
        let image = image::RgbImage::from_fn(1600, 1200, |_, _| {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let [a, b, c, _] = seed.to_le_bytes();
            image::Rgb([a, b, c])
        });
        let mut out = Cursor::new(Vec::new());
        image
            .write_to(&mut out, image::ImageFormat::Png)
            .expect("png");
        let bytes = out.into_inner();
        assert!(bytes.len() > PHOTO_MAX_BYTES);
        let photo = photo_from_bytes(&bytes).expect("a smaller photo");
        assert!(photo.width.max(photo.height) <= PHOTO_SIDES[0]);
        let shown = photo.sizes[0].bytes.as_ref().expect("bytes");
        assert!(shown.as_slice().len() <= PHOTO_MAX_BYTES);
        assert!(shown.as_slice().starts_with(&[0xFF, 0xD8]), "a JPEG");
        assert!(photo_from_bytes(b"not an image").is_none());
    }
}
