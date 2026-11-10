// SPDX-License-Identifier: AGPL-3.0-only
//! Images, stickers, audio, and video of Signal messages.
//!
//! A photo or a sticker downloads when its chat is open, and its row comes
//! again with the image. The image crosses the pipe as bytes, so a large
//! photo is made smaller first. A voice note, an audio file, or a video
//! downloads when the user presses Play. `Debug` shows no key, no path, and
//! no bytes.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;

use presage::proto::AttachmentPointer;
use thinwire_protocol::{ChatMessage, MessageAudio, MessagePhoto, MessageVideo};

/// The largest photo the helper downloads.
const PHOTO_DOWNLOAD_MAX: u32 = 25 * 1024 * 1024;

/// The largest sticker file the helper downloads.
const STICKER_DOWNLOAD_MAX: u32 = 2 * 1024 * 1024;

/// The longest side of a sticker in the thread, in points. The image keeps
/// its own pixels, so it stays sharp on a high-density screen.
const STICKER_SIDE: u32 = 160;

/// The body of a sticker message.
pub(super) const STICKER_LABEL: &str = "[sticker]";

/// The longest side of the blurred picture of a video, in pixels.
const BLUR_SIDE: u32 = 32;

/// The size of the picture of a video that does not say its size.
const VIDEO_SHOWN: (u32, u32) = (320, 180);

/// The flag of a GIF in [`AttachmentPointer::flags`]: a video with no
/// sound that plays again and again.
const GIF_FLAG: u32 = 8;

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
    Video,
    /// A video that Signal shows as a GIF. It plays as a video.
    Gif,
    File,
}

pub(super) fn kind(pointer: &AttachmentPointer) -> Kind {
    let content = pointer.content_type.as_deref().unwrap_or("");
    if content.starts_with("image/") {
        Kind::Photo
    } else if content.starts_with("video/") {
        if pointer.flags.unwrap_or(0) & GIF_FLAG != 0 {
            Kind::Gif
        } else {
            Kind::Video
        }
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
        Kind::Video => "[video]",
        Kind::Gif => "[GIF]",
        Kind::File => "[file]",
    }
}

/// The handle of the audio of one message for `DownloadVoice`, and of its
/// video for `DownloadVideo`. The same message has the same handle in every
/// run. Always positive.
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
    /// The image is a sticker. It shows small, and an animated one shows
    /// its first picture.
    pub sticker: bool,
}

impl PhotoJob {
    /// The photo of the row, from the downloaded file. `None` when the
    /// bytes are not an image that the app can draw.
    pub(super) fn photo(&self, bytes: &[u8]) -> Option<MessagePhoto> {
        if self.sticker {
            sticker_from_bytes(bytes)
        } else {
            photo_from_bytes(bytes)
        }
    }
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
    videos: HashMap<i32, AttachmentPointer>,
    /// The video handles with a download that runs.
    video_runs: HashSet<i32>,
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

    /// Set the photo, the audio, and the video of `message` from its
    /// attachments and its sticker.
    ///
    /// A photo in the cache shows at once, with `caption` as the body. A new
    /// photo waits in the queue of its chat, and the row keeps its label. A
    /// sticker is a small photo with no caption. A video keeps its label
    /// or its caption and gets a handle, and its picture is the photo.
    pub(super) fn dress(
        &self,
        message: &mut ChatMessage,
        attachments: &[AttachmentPointer],
        caption: Option<&str>,
        sticker: Option<&AttachmentPointer>,
    ) {
        let mut book = self.lock();
        let photo = attachments
            .iter()
            .find(|pointer| kind(pointer) == Kind::Photo);
        let image = photo
            .map(|pointer| (pointer, PHOTO_DOWNLOAD_MAX, false))
            .or_else(|| sticker.map(|pointer| (pointer, STICKER_DOWNLOAD_MAX, true)));
        if let Some((pointer, max, is_sticker)) = image {
            match book.photos.get(&message.id).cloned() {
                Some(PhotoState::Ready(photo)) => {
                    message.photo = Some(photo);
                    message.body = caption.unwrap_or("").to_string();
                }
                Some(PhotoState::Requested | PhotoState::Failed) => {}
                None if pointer.size.is_some_and(|size| size > max) => {}
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
                            sticker: is_sticker,
                        });
                }
            }
        }
        // A message with a photo shows the photo. Its video has no place
        // for a picture then, so only a message with no photo plays.
        let video = attachments
            .iter()
            .find(|pointer| matches!(kind(pointer), Kind::Video | Kind::Gif))
            .filter(|_| image.is_none())
            .filter(|pointer| {
                pointer.size.is_none_or(|size| {
                    usize::try_from(size)
                        .is_ok_and(|size| size <= thinwire_protocol::voice_files::VIDEO_MAX_BYTES)
                })
            });
        if let Some(pointer) = video {
            let file_id = voice_file_id(&message.id);
            let (width, height) = (pointer.width.unwrap_or(0), pointer.height.unwrap_or(0));
            // Signal does not send the length of a video.
            message.video = MessageVideo::new(0, width, height, file_id, None);
            if message.video.is_some() {
                book.videos.insert(file_id, pointer.clone());
                message.photo = video_picture(pointer);
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

    /// The attachment of the video `file_id`, when a row of it was shown.
    pub(super) fn video(&self, file_id: i32) -> Option<AttachmentPointer> {
        self.lock().videos.get(&file_id).cloned()
    }

    /// A download of the video `file_id` starts. `false`: one already runs,
    /// and its answer is the answer for this request too.
    pub(super) fn start_video(&self, file_id: i32) -> bool {
        self.lock().video_runs.insert(file_id)
    }

    /// The download of the video `file_id` ended.
    pub(super) fn end_video(&self, file_id: i32) {
        self.lock().video_runs.remove(&file_id);
    }
}

/// The picture of a video before it plays: the thumbnail of the attachment
/// when it is an image, else the blurred picture of its `blur_hash`. `None`
/// when the attachment has neither.
fn video_picture(pointer: &AttachmentPointer) -> Option<MessagePhoto> {
    use thinwire_protocol::photo_resize;
    let shown = match (pointer.width.unwrap_or(0), pointer.height.unwrap_or(0)) {
        (0, _) | (_, 0) => None,
        size => Some(size),
    };
    if let Some(bytes) = pointer.thumbnail.as_ref()
        && bytes.len() <= photo_resize::PHOTO_MAX_BYTES
        && let Some(size) = photo_resize::image_size(bytes)
    {
        let (width, height) = shown.unwrap_or(size);
        return Some(photo_resize::photo(width, height, bytes.clone()));
    }
    let hash = pointer.blur_hash.as_deref()?;
    // The blurred picture has the shape of the video.
    let (width, height) = shown.unwrap_or(VIDEO_SHOWN);
    let long = width.max(height);
    let side = |value: u32| {
        u32::try_from(u64::from(value) * u64::from(BLUR_SIDE) / u64::from(long))
            .unwrap_or(BLUR_SIDE)
            .clamp(1, BLUR_SIDE)
    };
    let (blur_width, blur_height) = (side(width), side(height));
    let pixels = super::blurhash::decode(hash, blur_width, blur_height)?;
    photo_resize::photo_from_rgb(blur_width, blur_height, pixels, (width, height))
}

/// The image of a sticker, at the size of a sticker in the thread. An
/// animated sticker shows its first picture.
fn sticker_from_bytes(bytes: &[u8]) -> Option<MessagePhoto> {
    use thinwire_protocol::photo_resize;
    let image = photo_resize::sticker_from_bytes(bytes)?;
    let (width, height) = photo_resize::image_size(&image)?;
    let long = width.max(height).max(1);
    let side = |value: u32| {
        u32::try_from(u64::from(value) * u64::from(STICKER_SIDE) / u64::from(long))
            .unwrap_or(STICKER_SIDE)
            .max(1)
    };
    Some(photo_resize::photo(side(width), side(height), image))
}

fn trim(book: &mut Book) {
    while book.order.len() > PHOTO_CACHE {
        if let Some(old) = book.order.pop_front() {
            book.photos.remove(&old);
        }
    }
}

/// The photo that crosses the pipe, from the downloaded file.
fn photo_from_bytes(bytes: &[u8]) -> Option<MessagePhoto> {
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
            edited: false,
        }
    }

    #[test]
    fn an_attachment_has_a_kind_and_a_label() {
        assert_eq!(kind(&pointer("image/jpeg", 0)), Kind::Photo);
        assert_eq!(kind(&pointer("audio/aac", 1)), Kind::Voice);
        assert_eq!(kind(&pointer("audio/mpeg", 0)), Kind::Audio);
        assert_eq!(kind(&pointer("application/pdf", 0)), Kind::File);
        assert_eq!(kind(&pointer("video/mp4", 0)), Kind::Video);
        assert_eq!(kind(&pointer("video/mp4", 8)), Kind::Gif);
        assert_eq!(label(Kind::Video), "[video]");
        assert_eq!(label(Kind::Gif), "[GIF]");
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
        book.dress(&mut row, &[pointer("image/png", 0)], Some("look"), None);
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
        book.dress(&mut again, &[pointer("image/png", 0)], None, None);
        assert!(again.photo.is_some());
        assert_eq!(again.body, "");
        assert!(book.take_jobs("signal:chat").is_empty(), "no new download");
    }

    /// A failed photo keeps the label and does not ask again.
    #[test]
    fn a_failed_photo_keeps_the_label() {
        let book = MediaBook::default();
        let mut row = message("m2");
        book.dress(&mut row, &[pointer("image/jpeg", 0)], None, None);
        let job = book.take_jobs("signal:chat").remove(0);
        assert!(book.finish(job, None).is_none());
        let mut again = message("m2");
        book.dress(&mut again, &[pointer("image/jpeg", 0)], None, None);
        assert!(again.photo.is_none());
        assert!(book.take_jobs("signal:chat").is_empty());
    }

    /// A voice note gets a handle. The handle finds its attachment.
    #[test]
    fn a_voice_note_has_a_handle_for_its_attachment() {
        let book = MediaBook::default();
        let mut row = message("m3");
        row.body = "[voice message]".into();
        book.dress(&mut row, &[pointer("audio/aac", 1)], None, None);
        let audio = row.audio.expect("audio");
        assert_eq!(audio.file_id(), voice_file_id("m3"));
        assert_eq!(audio.duration_secs(), 0);
        assert!(book.voice(audio.file_id()).is_some());
        assert!(book.voice(audio.file_id().wrapping_add(1).max(1)).is_none());
        assert!(row.video.is_none());
        assert!(book.video(audio.file_id()).is_none());
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let shown = thinwire_protocol::photo_resize::photo_from_rgb(
            width,
            height,
            vec![90; (width * height * 3) as usize],
            (width, height),
        )
        .expect("an image");
        shown.sizes[0]
            .bytes
            .as_ref()
            .expect("bytes")
            .as_slice()
            .to_vec()
    }

    /// A video gets a handle and keeps its label. Its picture is the
    /// blurred picture of the attachment, at the size of the video. A GIF
    /// is a video. A video is never a video message: Signal has none.
    #[test]
    fn a_video_has_a_handle_and_a_blurred_picture() {
        let book = MediaBook::default();
        let mut row = message("m4");
        row.body = "[video]".into();
        let clip = AttachmentPointer {
            width: Some(640),
            height: Some(360),
            blur_hash: Some("LEHV6nWB2yk8pyo0adR*.7kCMdnj".into()),
            ..pointer("video/mp4", 0)
        };
        book.dress(&mut row, std::slice::from_ref(&clip), None, None);
        assert_eq!(row.body, "[video]");
        let video = row.video.clone().expect("video");
        assert_eq!(video.file_id(), voice_file_id("m4"));
        assert_eq!(video.size(), (640, 360));
        assert_eq!(video.duration_secs(), 0, "Signal does not send the length");
        assert!(!video.is_note());
        assert!(video.file().is_none());
        let photo = row.photo.expect("the blurred picture");
        assert_eq!((photo.width, photo.height), (640, 360));
        let bytes = photo.sizes[0].bytes.as_ref().expect("bytes");
        assert_eq!(
            thinwire_protocol::photo_resize::image_size(bytes.as_slice()),
            Some((32, 18))
        );
        assert!(book.video(video.file_id()).is_some());
        assert!(book.voice(video.file_id()).is_none());
        assert!(book.take_jobs("signal:chat").is_empty(), "no download yet");

        // One download runs for each handle at a time.
        assert!(book.start_video(video.file_id()));
        assert!(!book.start_video(video.file_id()));
        book.end_video(video.file_id());
        assert!(book.start_video(video.file_id()));

        // A GIF with a caption, with no picture: the row has no photo.
        let mut gif = message("m5");
        gif.body = "look".into();
        book.dress(&mut gif, &[pointer("video/mp4", 8)], Some("look"), None);
        assert_eq!(gif.body, "look");
        assert!(gif.video.is_some());
        assert!(gif.photo.is_none());

        // The thumbnail of the attachment wins over the blurred picture.
        let mut thumbed = message("m6");
        let with_thumbnail = AttachmentPointer {
            thumbnail: Some(png(4, 2)),
            ..clip
        };
        book.dress(&mut thumbed, &[with_thumbnail], None, None);
        let photo = thumbed.photo.expect("the thumbnail");
        assert_eq!((photo.width, photo.height), (640, 360));
        let bytes = photo.sizes[0].bytes.as_ref().expect("bytes");
        assert_eq!(
            thinwire_protocol::photo_resize::image_size(bytes.as_slice()),
            Some((4, 2))
        );
    }

    /// A video that is too large keeps its label only. A video beside a
    /// photo does not play: the row shows the photo.
    #[test]
    fn a_large_video_and_a_video_beside_a_photo_do_not_play() {
        let book = MediaBook::default();
        let mut row = message("m7");
        let large = AttachmentPointer {
            size: Some(u32::MAX),
            ..pointer("video/mp4", 0)
        };
        book.dress(&mut row, &[large], None, None);
        assert!(row.video.is_none());
        assert!(book.video(voice_file_id("m7")).is_none());

        let mut album = message("m8");
        book.dress(
            &mut album,
            &[pointer("image/jpeg", 0), pointer("video/mp4", 0)],
            None,
            None,
        );
        assert!(album.video.is_none());
        assert_eq!(
            book.take_jobs("signal:chat").len(),
            1,
            "the photo downloads"
        );
    }

    /// A sticker waits for its chat as a photo does. Its row comes back
    /// with a small image and no label. A sticker that is too large, and a
    /// file that is not an image, keep "[sticker]".
    #[test]
    fn a_sticker_shows_as_a_small_image() {
        let book = MediaBook::default();
        let mut row = message("m9");
        row.body = STICKER_LABEL.into();
        let sticker = pointer("image/webp", 0);
        book.dress(&mut row, &[], None, Some(&sticker));
        assert_eq!(row.body, "[sticker]");
        assert!(row.photo.is_none());
        let job = book.take_jobs("signal:chat").remove(0);
        assert!(job.sticker);
        let photo = job.photo(&png(512, 256)).expect("an image");
        assert_eq!((photo.width, photo.height), (160, 80));
        assert!(job.photo(b"not an image").is_none());
        let shown = book.finish(job, Some(photo)).expect("the row comes back");
        assert_eq!(shown.body, "");
        assert!(shown.photo.is_some());

        let mut large = message("m10");
        large.body = STICKER_LABEL.into();
        let huge = AttachmentPointer {
            size: Some(STICKER_DOWNLOAD_MAX + 1),
            ..pointer("image/webp", 0)
        };
        book.dress(&mut large, &[], None, Some(&huge));
        assert!(book.take_jobs("signal:chat").is_empty());

        // A photo job is not a sticker: it keeps the size of its image.
        let mut plain = message("m11");
        book.dress(&mut plain, &[pointer("image/png", 0)], None, None);
        let job = book.take_jobs("signal:chat").remove(0);
        assert!(!job.sticker);
        let photo = job.photo(&png(512, 256)).expect("an image");
        assert_eq!((photo.width, photo.height), (512, 256));
    }
}
