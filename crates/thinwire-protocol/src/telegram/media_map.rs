//! Turn a TDLib message into a caption, links, a photo, a voice note, a clip,
//! a sticker, or a document.
//!
//! Offsets from TDLib are UTF-16. [`crate::links_in`] stores UTF-8 indexes.

use crate::media::{
    AudioPath, MessageAudio, MessageFile, MessageLink, MessagePhoto, MessageVideo, PhotoSize,
    TextKind, TextSpan, links_in,
};
use crate::{ImageBytes, ImagePath};

pub(super) struct Mapped {
    pub text: String,
    pub preview: String,
    pub links: Vec<MessageLink>,
    pub photo: Option<MessagePhoto>,
    pub audio: Option<MessageAudio>,
    /// A clip for the in-app player. `photo` is then its thumbnail.
    pub video: Option<MessageVideo>,
    /// A document. `photo` stays empty. A preview lives on the file.
    pub file: Option<MessageFile>,
    /// Plain text with no entities: the worker asks `getTextEntities`.
    pub scan: bool,
}

pub(super) fn map_content(content: &tdlib_rs::enums::MessageContent) -> Mapped {
    match content {
        tdlib_rs::enums::MessageContent::MessageText(text) => {
            let (body, links, scan) = from_formatted(&text.text);
            Mapped {
                preview: body.clone(),
                text: body,
                links,
                photo: None,
                audio: None,
                video: None,
                file: None,
                scan,
            }
        }
        tdlib_rs::enums::MessageContent::MessagePhoto(photo) => {
            let (caption, links, scan) = from_formatted(&photo.caption);
            let mapped = map_photo(photo);
            let preview = if caption.is_empty() {
                "Photo".to_string()
            } else {
                caption.clone()
            };
            Mapped {
                text: caption,
                preview,
                links,
                photo: Some(mapped),
                audio: None,
                video: None,
                file: None,
                scan,
            }
        }
        tdlib_rs::enums::MessageContent::MessageSticker(message) => map_sticker(&message.sticker),
        // Telegram draws one emoji as an animation. The app draws the emoji
        // itself, large.
        tdlib_rs::enums::MessageContent::MessageAnimatedEmoji(message)
            if !message.emoji.trim().is_empty() =>
        {
            label(message.emoji.trim())
        }
        tdlib_rs::enums::MessageContent::MessageDice(dice) => map_dice(dice),
        tdlib_rs::enums::MessageContent::MessageVideo(message) => map_clip(
            &Clip {
                label: "Video",
                duration: message.video.duration,
                width: message.video.width,
                height: message.video.height,
                minithumbnail: message.video.minithumbnail.as_ref(),
                thumbnail: message.video.thumbnail.as_ref(),
                file: &message.video.video,
                secret: message.is_secret,
                spoiler: message.has_spoiler,
                note: false,
            },
            Some(&message.caption),
        ),
        tdlib_rs::enums::MessageContent::MessageAnimation(message) => map_clip(
            &Clip {
                label: "Animation",
                duration: message.animation.duration,
                width: message.animation.width,
                height: message.animation.height,
                minithumbnail: message.animation.minithumbnail.as_ref(),
                thumbnail: message.animation.thumbnail.as_ref(),
                file: &message.animation.animation,
                secret: message.is_secret,
                spoiler: message.has_spoiler,
                note: false,
            },
            Some(&message.caption),
        ),
        tdlib_rs::enums::MessageContent::MessageVideoNote(message) => map_clip(
            &Clip {
                label: "Video message",
                duration: message.video_note.duration,
                width: message.video_note.length,
                height: message.video_note.length,
                minithumbnail: message.video_note.minithumbnail.as_ref(),
                thumbnail: message.video_note.thumbnail.as_ref(),
                file: &message.video_note.video,
                secret: message.is_secret,
                spoiler: false,
                note: true,
            },
            None,
        ),
        tdlib_rs::enums::MessageContent::MessageAudio(message) => map_music(message),
        // This TDLib build has no `is_secret` on a voice note. An expired
        // voice note is `MessageExpiredVoiceNote` and stays on the label arm.
        tdlib_rs::enums::MessageContent::MessageVoiceNote(note) => map_voice(note),
        tdlib_rs::enums::MessageContent::MessageDocument(message) => map_document(message),
        tdlib_rs::enums::MessageContent::MessageCall(call) => label(&call_label(call)),
        tdlib_rs::enums::MessageContent::MessageLocation(_) => label("Location"),
        tdlib_rs::enums::MessageContent::MessageContact(message) => {
            let contact = &message.contact;
            let name = format!("{} {}", contact.first_name, contact.last_name);
            match name.trim() {
                "" => label("Contact"),
                name => label(&format!("Contact: {name}")),
            }
        }
        tdlib_rs::enums::MessageContent::MessagePoll(message) => {
            match message.poll.question.text.trim() {
                "" => label("Poll"),
                question => label(&format!("Poll: {question}")),
            }
        }
        _ => label("Message"),
    }
}

fn label(text: &str) -> Mapped {
    Mapped {
        text: text.to_string(),
        preview: text.to_string(),
        links: Vec::new(),
        photo: None,
        audio: None,
        video: None,
        file: None,
        scan: false,
    }
}

/// The longest side of a sticker in its row, in pixels. A photo fills a
/// larger box.
const STICKER_SIDE: u32 = 192;

/// `true` for a thumbnail that the app draws as an image.
fn is_image(format: &tdlib_rs::enums::ThumbnailFormat) -> bool {
    matches!(
        format,
        tdlib_rs::enums::ThumbnailFormat::Jpeg
            | tdlib_rs::enums::ThumbnailFormat::Png
            | tdlib_rs::enums::ThumbnailFormat::Webp
    )
}

/// A sticker. A static sticker is a WebP image: the row shows its file. An
/// animated sticker (TGS) and a video sticker (WebM) show their thumbnail.
/// A video sticker is also a clip that plays in its row. The emoji of the
/// sticker is the text of the row. A sticker with no image that the app
/// draws keeps the label "Sticker".
fn map_sticker(sticker: &tdlib_rs::types::Sticker) -> Mapped {
    let emoji = sticker.emoji.trim();
    let named = if emoji.is_empty() {
        "Sticker".to_string()
    } else {
        format!("{emoji} Sticker")
    };
    let mut mapped = label(&named);
    let size = |value: i32| u32::try_from(value).unwrap_or(0);
    let mut sizes: Vec<PhotoSize> = sticker
        .thumbnail
        .as_ref()
        .filter(|thumbnail| is_image(&thumbnail.format))
        .map(|thumbnail| PhotoSize {
            width: size(thumbnail.width),
            height: size(thumbnail.height),
            file_id: (thumbnail.file.id > 0).then_some(thumbnail.file.id),
            path: local_path(&thumbnail.file, false),
            bytes: None,
        })
        .into_iter()
        .collect();
    let (width, height) = (size(sticker.width), size(sticker.height));
    if matches!(sticker.format, tdlib_rs::enums::StickerFormat::Webp) && sticker.sticker.id > 0 {
        sizes.push(PhotoSize {
            width,
            height,
            file_id: Some(sticker.sticker.id),
            path: local_path(&sticker.sticker, false),
            bytes: None,
        });
    }
    sizes.retain(|size| size.file_id.is_some() || size.path.is_some());
    let (box_width, box_height) = sticker_box(width, height, &sizes);
    if matches!(sticker.format, tdlib_rs::enums::StickerFormat::Webm) {
        let local = sticker
            .sticker
            .local
            .is_downloading_completed
            .then(|| AudioPath::new(sticker.sticker.local.path.clone()))
            .flatten();
        // The clip has the size of the row picture, so the row does not
        // move when the clip plays in the place of the thumbnail.
        mapped.video = MessageVideo::new(0, box_width, box_height, sticker.sticker.id, local)
            .map(MessageVideo::into_note);
    }
    if sizes.is_empty() {
        return mapped;
    }
    mapped.photo = Some(MessagePhoto {
        width: box_width,
        height: box_height,
        mini: None,
        sizes,
        has_spoiler: false,
        is_secret: false,
        failed: false,
    });
    // The image shows the sticker: only its emoji stays as the text.
    mapped.text = emoji.to_string();
    mapped
}

/// The size of the row picture of a sticker: its shape, with the longest
/// side at [`STICKER_SIDE`] at most. A sticker with no size has the shape of
/// its largest image.
fn sticker_box(width: u32, height: u32, sizes: &[PhotoSize]) -> (u32, u32) {
    let (width, height) = if width > 0 && height > 0 {
        (width, height)
    } else {
        sizes
            .iter()
            .max_by_key(|size| u64::from(size.width) * u64::from(size.height))
            .map_or((0, 0), |size| (size.width, size.height))
    };
    let long = width.max(height);
    if long <= STICKER_SIDE {
        return (width, height);
    }
    let scale = |side: u32| {
        let scaled = u64::from(side) * u64::from(STICKER_SIDE) / u64::from(long);
        u32::try_from(scaled).unwrap_or(STICKER_SIDE).max(1)
    };
    (scale(width), scale(height))
}

/// A call in a one-to-one chat: its kind, how it ended, and its length.
fn call_label(call: &tdlib_rs::types::MessageCall) -> String {
    use tdlib_rs::enums::CallDiscardReason;
    let kind = if call.is_video { "Video call" } else { "Call" };
    match call.discard_reason {
        CallDiscardReason::Missed => format!("{kind}, not answered"),
        CallDiscardReason::Declined => format!("{kind}, declined"),
        _ if call.duration > 0 => {
            format!("{kind}, {}:{:02}", call.duration / 60, call.duration % 60)
        }
        _ => kind.to_owned(),
    }
}

/// A dice message: its emoji and the value. The value is zero until the
/// throw ends.
fn map_dice(dice: &tdlib_rs::types::MessageDice) -> Mapped {
    let emoji = dice.emoji.trim();
    match (emoji.is_empty(), dice.value > 0) {
        (true, _) => label("Message"),
        (false, true) => label(&format!("{emoji} {}", dice.value)),
        (false, false) => label(emoji),
    }
}

/// The emoji reactions of a message. A custom emoji or a paid reaction is
/// not shown: the app draws plain emoji only.
pub(super) fn reactions_of(
    info: Option<&tdlib_rs::types::MessageInteractionInfo>,
) -> Vec<crate::Reaction> {
    let Some(reactions) = info.and_then(|info| info.reactions.as_ref()) else {
        return Vec::new();
    };
    reactions
        .reactions
        .iter()
        .filter_map(|reaction| match &reaction.r#type {
            tdlib_rs::enums::ReactionType::Emoji(emoji) => Some(crate::Reaction {
                emoji: emoji.emoji.clone(),
                count: u32::try_from(reaction.total_count).unwrap_or(0),
                mine: reaction.is_chosen,
            }),
            _ => None,
        })
        .filter(|reaction| reaction.count > 0)
        .collect()
}

/// The row id of the message that `message` replies to, in the same chat.
pub(super) fn reply_of(message: &tdlib_rs::types::Message) -> Option<String> {
    match message.reply_to.as_ref()? {
        tdlib_rs::enums::MessageReplyTo::Message(reply) if reply.chat_id == message.chat_id => {
            Some(super::inbox::message_id(reply.chat_id, reply.message_id))
        }
        _ => None,
    }
}

/// The parts of a video, an animation, or a video note.
struct Clip<'a> {
    label: &'static str,
    duration: i32,
    width: i32,
    height: i32,
    minithumbnail: Option<&'a tdlib_rs::types::Minithumbnail>,
    thumbnail: Option<&'a tdlib_rs::types::Thumbnail>,
    file: &'a tdlib_rs::types::File,
    secret: bool,
    spoiler: bool,
    /// A video message (TDLib "video note"). It plays in its row.
    note: bool,
}

/// A clip: its thumbnail is the photo of the row, and the file plays in the
/// app (ADR 0015). A secret clip keeps its label only.
fn map_clip(clip: &Clip<'_>, caption: Option<&tdlib_rs::types::FormattedText>) -> Mapped {
    let (text, links, scan) =
        caption.map_or_else(|| (String::new(), Vec::new(), false), from_formatted);
    let mut mapped = label(clip.label);
    if !text.is_empty() {
        mapped.preview.clone_from(&text);
        mapped.text = text;
        mapped.links = links;
        mapped.scan = scan;
    }
    if clip.secret {
        return mapped;
    }
    let size = |value: i32| u32::try_from(value).unwrap_or(0);
    let (width, height) = (size(clip.width), size(clip.height));
    let sizes: Vec<PhotoSize> = clip
        .thumbnail
        .filter(|thumbnail| is_image(&thumbnail.format))
        .map(|thumbnail| PhotoSize {
            width: size(thumbnail.width),
            height: size(thumbnail.height),
            file_id: (thumbnail.file.id > 0).then_some(thumbnail.file.id),
            path: local_path(&thumbnail.file, false),
            bytes: None,
        })
        .into_iter()
        .collect();
    let mini = clip
        .minithumbnail
        .and_then(|mini| thumbnail_bytes(&mini.data))
        .map(ImageBytes::new);
    if !sizes.is_empty() || mini.is_some() {
        mapped.photo = Some(MessagePhoto {
            width,
            height,
            mini,
            sizes,
            has_spoiler: clip.spoiler,
            is_secret: false,
            failed: false,
        });
    }
    let local = clip
        .file
        .local
        .is_downloading_completed
        .then(|| AudioPath::new(clip.file.local.path.clone()))
        .flatten();
    mapped.video = MessageVideo::new(size(clip.duration), width, height, clip.file.id, local)
        .map(|video| if clip.note { video.into_note() } else { video });
    mapped
}

/// A music file: it plays as a voice note does.
fn map_music(message: &tdlib_rs::types::MessageAudio) -> Mapped {
    let (text, links, scan) = from_formatted(&message.caption);
    let mut mapped = label("Audio");
    if !text.is_empty() {
        mapped.preview.clone_from(&text);
        mapped.text = text;
        mapped.links = links;
        mapped.scan = scan;
    }
    let file = &message.audio.audio;
    let local = file
        .local
        .is_downloading_completed
        .then(|| AudioPath::new(file.local.path.clone()))
        .flatten();
    mapped.audio = MessageAudio::telegram(message.audio.duration, file.id, local);
    mapped
}

fn map_voice(note: &tdlib_rs::types::MessageVoiceNote) -> Mapped {
    let mut mapped = label("Voice message");
    let local = note
        .voice_note
        .voice
        .local
        .is_downloading_completed
        .then(|| AudioPath::new(note.voice_note.voice.local.path.clone()))
        .flatten();
    mapped.audio =
        MessageAudio::telegram(note.voice_note.duration, note.voice_note.voice.id, local);
    mapped
}

/// A thumbnail or minithumbnail is the preview. The app does not render a page.
fn map_document(message: &tdlib_rs::types::MessageDocument) -> Mapped {
    let (text, links, scan) = from_formatted(&message.caption);
    let doc = &message.document;
    let local = doc
        .document
        .local
        .is_downloading_completed
        .then(|| AudioPath::new(doc.document.local.path.clone()))
        .flatten();
    let file = MessageFile::new(
        doc.file_name.clone(),
        doc.mime_type.clone(),
        byte_size(&doc.document),
        doc.document.id,
        local,
        document_preview(doc),
    );
    let mut mapped = label("File");
    mapped.preview = if text.is_empty() {
        file.name().to_string()
    } else {
        text.clone()
    };
    mapped.text = text;
    mapped.links = links;
    mapped.scan = scan;
    mapped.file = Some(file);
    mapped
}

fn byte_size(file: &tdlib_rs::types::File) -> u64 {
    let size = file.size.max(file.expected_size);
    u64::try_from(size).unwrap_or(0)
}

fn document_preview(doc: &tdlib_rs::types::Document) -> Option<MessagePhoto> {
    let mini_src = doc.minithumbnail.as_ref();
    let mini = mini_src
        .and_then(|mini| thumbnail_bytes(&mini.data))
        .map(ImageBytes::new);
    let size = |value: i32| u32::try_from(value).unwrap_or(0);
    let thumb = doc
        .thumbnail
        .as_ref()
        .filter(|thumbnail| is_image(&thumbnail.format))
        .map(|thumbnail| PhotoSize {
            width: size(thumbnail.width),
            height: size(thumbnail.height),
            file_id: (thumbnail.file.id > 0).then_some(thumbnail.file.id),
            path: local_path(&thumbnail.file, false),
            bytes: None,
        });
    if mini.is_none() && thumb.is_none() {
        return None;
    }
    let (width, height) = thumb
        .as_ref()
        .map(|size| (size.width, size.height))
        .filter(|(width, height)| *width > 0 && *height > 0)
        .or_else(|| mini_src.map(|mini| (size(mini.width), size(mini.height))))
        .unwrap_or((0, 0));
    Some(MessagePhoto {
        width,
        height,
        mini,
        sizes: thumb.into_iter().collect(),
        has_spoiler: false,
        is_secret: false,
        failed: false,
    })
}

fn from_formatted(text: &tdlib_rs::types::FormattedText) -> (String, Vec<MessageLink>, bool) {
    let links = links_from(&text.text, &text.entities);
    let scan = text.entities.is_empty() && !text.text.is_empty();
    (text.text.clone(), links, scan)
}

pub(super) fn links_from(text: &str, entities: &[tdlib_rs::types::TextEntity]) -> Vec<MessageLink> {
    let spans = entities.iter().filter_map(|entity| {
        let kind = match &entity.r#type {
            tdlib_rs::enums::TextEntityType::Url => TextKind::Url,
            tdlib_rs::enums::TextEntityType::EmailAddress => TextKind::Email,
            tdlib_rs::enums::TextEntityType::TextUrl(url) => TextKind::TextUrl(url.url.clone()),
            _ => return None,
        };
        Some(TextSpan {
            offset: entity.offset,
            length: entity.length,
            kind,
        })
    });
    links_in(text, &spans.collect::<Vec<_>>())
}

fn map_photo(photo: &tdlib_rs::types::MessagePhoto) -> MessagePhoto {
    let sizes: Vec<PhotoSize> = photo
        .photo
        .sizes
        .iter()
        .map(|size| PhotoSize {
            width: u32::try_from(size.width).unwrap_or(0),
            height: u32::try_from(size.height).unwrap_or(0),
            file_id: (size.photo.id > 0).then_some(size.photo.id),
            path: local_path(&size.photo, photo.is_secret),
            bytes: None,
        })
        .collect();
    let (width, height) = sizes
        .iter()
        .max_by_key(|size| u64::from(size.width) * u64::from(size.height))
        .map(|size| (size.width, size.height))
        .unwrap_or((0, 0));
    MessagePhoto {
        width,
        height,
        mini: photo
            .photo
            .minithumbnail
            .as_ref()
            .and_then(|mini| thumbnail_bytes(&mini.data))
            .map(ImageBytes::new),
        sizes,
        has_spoiler: photo.has_spoiler,
        is_secret: photo.is_secret,
        failed: false,
    }
}

/// TDLib JSON stores the JPEG bytes as base64.
fn thumbnail_bytes(data: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.trim())
        .ok()?;
    (!bytes.is_empty()).then_some(bytes)
}

fn local_path(file: &tdlib_rs::types::File, secret: bool) -> Option<ImagePath> {
    if secret || !file.local.is_downloading_completed || file.local.path.is_empty() {
        None
    } else {
        Some(ImagePath::new(file.local.path.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::{map_content, thumbnail_bytes};

    #[test]
    fn a_minithumbnail_is_base64_jpeg_bytes() {
        let bytes = thumbnail_bytes("/9j/").expect("jpeg");
        assert_eq!(bytes, vec![0xff, 0xd8, 0xff]);
        assert!(thumbnail_bytes("").is_none());
        assert!(thumbnail_bytes("!!!!").is_none());
    }

    fn voice(
        file_id: i32,
        duration: i32,
        completed: bool,
        path: &str,
    ) -> tdlib_rs::enums::MessageContent {
        let mut note = tdlib_rs::types::MessageVoiceNote::default();
        note.voice_note.duration = duration;
        note.voice_note.voice.id = file_id;
        note.voice_note.voice.local.is_downloading_completed = completed;
        note.voice_note.voice.local.path = path.to_string();
        tdlib_rs::enums::MessageContent::MessageVoiceNote(note)
    }

    #[test]
    fn a_voice_note_and_music_play_and_a_secret_clip_stays_a_label() {
        let mapped = map_content(&voice(42, 15, false, ""));
        assert_eq!(mapped.text, "Voice message");
        assert_eq!(mapped.preview, "Voice message");
        let audio = mapped.audio.expect("voice note");
        assert_eq!(audio.file_id(), 42);
        assert_eq!(audio.duration_secs(), 15);
        assert!(audio.file().is_none());
        assert!(mapped.photo.is_none());

        let ready = map_content(&voice(42, 15, true, "/tmp/hidden-voice.ogg"));
        let audio = ready.audio.expect("ready");
        assert_eq!(
            audio.file().expect("path").as_path(),
            std::path::Path::new("/tmp/hidden-voice.ogg")
        );
        assert!(!format!("{audio:?}").contains("hidden-voice"));

        let missing = map_content(&voice(0, 15, false, ""));
        assert!(missing.audio.is_none());
        assert_eq!(missing.preview, "Voice message");

        let music = tdlib_rs::enums::MessageContent::MessageAudio(tdlib_rs::types::MessageAudio {
            audio: tdlib_rs::types::Audio {
                duration: 20,
                title: "Song".into(),
                performer: String::new(),
                file_name: "song.mp3".into(),
                mime_type: "audio/mpeg".into(),
                album_cover_minithumbnail: None,
                album_cover_thumbnail: None,
                external_album_covers: Vec::new(),
                audio: tdlib_rs::types::File {
                    id: 9,
                    ..tdlib_rs::types::File::default()
                },
            },
            caption: tdlib_rs::types::FormattedText::default(),
        });
        // Music plays as a voice note does.
        let music = map_content(&music);
        assert_eq!(music.audio.expect("music").file_id(), 9);
        assert_eq!(music.preview, "Audio");

        let note =
            tdlib_rs::enums::MessageContent::MessageVideoNote(tdlib_rs::types::MessageVideoNote {
                video_note: tdlib_rs::types::VideoNote {
                    duration: 3,
                    waveform: String::new(),
                    length: 240,
                    minithumbnail: None,
                    thumbnail: None,
                    speech_recognition_result: None,
                    video: tdlib_rs::types::File {
                        id: 8,
                        ..tdlib_rs::types::File::default()
                    },
                },
                is_viewed: false,
                is_secret: true,
            });
        // A video message plays in its row: its clip is a note.
        let mut open_note = note.clone();
        if let tdlib_rs::enums::MessageContent::MessageVideoNote(message) = &mut open_note {
            message.is_secret = false;
        }
        let clip = map_content(&open_note).video.expect("clip");
        assert!(clip.is_note());
        assert_eq!((clip.file_id(), clip.size()), (8, (240, 240)));
        // A secret video note keeps its label: no file plays in the app.
        let note = map_content(&note);
        assert!(note.audio.is_none());
        assert!(note.video.is_none());
        assert_eq!(note.preview, "Video message");

        let expired = map_content(&tdlib_rs::enums::MessageContent::MessageExpiredVoiceNote);
        assert!(expired.audio.is_none());
        assert_eq!(expired.preview, "Message");
    }

    /// A video has its thumbnail as the photo, the blurred minithumbnail,
    /// and a clip with the size and the file of the video. Its caption is the
    /// body. A GIF thumbnail is not a photo.
    #[test]
    fn a_video_has_a_thumbnail_and_a_clip() {
        let file = |id: i32| tdlib_rs::types::File {
            id,
            ..tdlib_rs::types::File::default()
        };
        let mut message = tdlib_rs::types::MessageVideo {
            video: tdlib_rs::types::Video {
                duration: 12,
                width: 1280,
                height: 720,
                file_name: "lake.mp4".into(),
                mime_type: "video/mp4".into(),
                has_stickers: false,
                supports_streaming: true,
                minithumbnail: Some(tdlib_rs::types::Minithumbnail {
                    width: 40,
                    height: 22,
                    data: "/9j/".into(),
                }),
                thumbnail: Some(tdlib_rs::types::Thumbnail {
                    format: tdlib_rs::enums::ThumbnailFormat::Jpeg,
                    width: 320,
                    height: 180,
                    file: file(32),
                }),
                video: file(31),
            },
            alternative_videos: Vec::new(),
            storyboards: Vec::new(),
            cover: None,
            start_timestamp: 0,
            caption: tdlib_rs::types::FormattedText {
                text: "at the lake".into(),
                ..tdlib_rs::types::FormattedText::default()
            },
            show_caption_above_media: false,
            has_spoiler: false,
            is_secret: false,
        };
        let mapped = map_content(&tdlib_rs::enums::MessageContent::MessageVideo(
            message.clone(),
        ));
        assert_eq!(mapped.text, "at the lake");
        let video = mapped.video.expect("clip");
        assert_eq!(
            (video.file_id(), video.duration_secs(), video.size()),
            (31, 12, (1280, 720))
        );
        assert!(video.file().is_none());
        assert!(!video.is_note(), "a video file plays over the window");
        let photo = mapped.photo.expect("thumbnail");
        assert_eq!((photo.width, photo.height), (1280, 720));
        assert_eq!(photo.sizes.len(), 1);
        assert_eq!(photo.sizes[0].file_id, Some(32));
        assert!(photo.mini.is_some());

        message.video.thumbnail = Some(tdlib_rs::types::Thumbnail {
            format: tdlib_rs::enums::ThumbnailFormat::Gif,
            width: 320,
            height: 180,
            file: file(33),
        });
        message.video.minithumbnail = None;
        message.caption.text.clear();
        let no_photo = map_content(&tdlib_rs::enums::MessageContent::MessageVideo(message));
        assert!(no_photo.photo.is_none(), "a GIF thumbnail is not drawn");
        assert!(no_photo.video.is_some());
        assert_eq!(no_photo.preview, "Video");
    }

    fn sticker(
        format: tdlib_rs::enums::StickerFormat,
        thumbnail: Option<tdlib_rs::enums::ThumbnailFormat>,
    ) -> tdlib_rs::types::Sticker {
        let file = |id: i32| tdlib_rs::types::File {
            id,
            ..tdlib_rs::types::File::default()
        };
        tdlib_rs::types::Sticker {
            id: 1,
            set_id: 2,
            width: 512,
            height: 256,
            emoji: "\u{1F600}".into(),
            format,
            full_type: tdlib_rs::enums::StickerFullType::Regular(
                tdlib_rs::types::StickerFullTypeRegular {
                    premium_animation: None,
                },
            ),
            thumbnail: thumbnail.map(|format| tdlib_rs::types::Thumbnail {
                format,
                width: 128,
                height: 64,
                file: file(52),
            }),
            sticker: file(51),
        }
    }

    fn sticker_message(sticker: tdlib_rs::types::Sticker) -> tdlib_rs::enums::MessageContent {
        tdlib_rs::enums::MessageContent::MessageSticker(tdlib_rs::types::MessageSticker {
            sticker,
            is_premium: false,
        })
    }

    /// A static sticker is an image: its thumbnail and its WebP file are
    /// the sizes. The row has the emoji as its text, and no label.
    #[test]
    fn a_static_sticker_is_an_image_with_its_emoji() {
        use tdlib_rs::enums::{StickerFormat, ThumbnailFormat};

        let mut webp = sticker(StickerFormat::Webp, Some(ThumbnailFormat::Webp));
        let mapped = map_content(&sticker_message(webp.clone()));
        assert_eq!(mapped.text, "\u{1F600}");
        assert_eq!(mapped.preview, "\u{1F600} Sticker");
        assert!(mapped.video.is_none());
        assert!(mapped.links.is_empty() && !mapped.scan);
        let photo = mapped.photo.expect("the sticker image");
        // The shape of 512 x 256, with the longest side at 192.
        assert_eq!((photo.width, photo.height), (192, 96));
        let ids: Vec<_> = photo.sizes.iter().map(|size| size.file_id).collect();
        assert_eq!(ids, vec![Some(52), Some(51)]);
        assert_eq!((photo.sizes[1].width, photo.sizes[1].height), (512, 256));
        assert!(photo.sizes.iter().all(|size| size.path.is_none()));
        assert!(!photo.has_spoiler && !photo.is_secret);

        // A downloaded file shows at once. No thumbnail is needed.
        webp.thumbnail = None;
        webp.sticker.local.is_downloading_completed = true;
        webp.sticker.local.path = "/tmp/hidden-sticker.webp".into();
        webp.emoji.clear();
        let mapped = map_content(&sticker_message(webp));
        assert_eq!(mapped.text, "");
        assert_eq!(mapped.preview, "Sticker");
        let photo = mapped.photo.expect("the sticker image");
        assert_eq!(photo.sizes.len(), 1);
        assert_eq!(
            photo.sizes[0].path.as_ref().map(crate::ImagePath::as_str),
            Some("/tmp/hidden-sticker.webp")
        );
        assert!(!format!("{photo:?}").contains("hidden-sticker"));
    }

    /// An animated sticker shows its thumbnail. A video sticker shows its
    /// thumbnail and plays in its row. A sticker with no image that the app
    /// draws keeps its label.
    #[test]
    fn an_animated_and_a_video_sticker_show_the_thumbnail() {
        use tdlib_rs::enums::{StickerFormat, ThumbnailFormat};

        let tgs = map_content(&sticker_message(sticker(
            StickerFormat::Tgs,
            Some(ThumbnailFormat::Webp),
        )));
        assert_eq!(tgs.text, "\u{1F600}");
        let photo = tgs.photo.expect("the thumbnail");
        assert_eq!(photo.sizes.len(), 1, "the TGS file is not an image");
        assert_eq!(photo.sizes[0].file_id, Some(52));
        assert_eq!((photo.width, photo.height), (192, 96));
        assert!(tgs.video.is_none());

        let webm = map_content(&sticker_message(sticker(
            StickerFormat::Webm,
            Some(ThumbnailFormat::Jpeg),
        )));
        assert_eq!(webm.text, "\u{1F600}");
        let photo = webm.photo.expect("the thumbnail");
        assert_eq!(photo.sizes.len(), 1, "the WebM file is not an image");
        let clip = webm.video.expect("the video sticker plays");
        assert!(clip.is_note(), "it plays in its row");
        // The clip has the size of the thumbnail in the row.
        assert_eq!((clip.file_id(), clip.size()), (51, (192, 96)));

        // A TGS thumbnail is not an image: the label stays.
        let bare = map_content(&sticker_message(sticker(
            StickerFormat::Tgs,
            Some(ThumbnailFormat::Tgs),
        )));
        assert!(bare.photo.is_none() && bare.video.is_none());
        assert_eq!(bare.text, "\u{1F600} Sticker");
        let none = map_content(&sticker_message(sticker(StickerFormat::Tgs, None)));
        assert!(none.photo.is_none());
        assert_eq!(none.text, "\u{1F600} Sticker");
        // A video sticker with no thumbnail has its label and its clip.
        let clip_only = map_content(&sticker_message(sticker(StickerFormat::Webm, None)));
        assert!(clip_only.photo.is_none());
        assert!(clip_only.video.is_some());
        assert_eq!(clip_only.text, "\u{1F600} Sticker");
    }

    /// An animated emoji is its emoji, and a dice is its emoji and value.
    #[test]
    fn an_animated_emoji_and_a_dice_are_text() {
        let emoji = tdlib_rs::enums::MessageContent::MessageAnimatedEmoji(
            tdlib_rs::types::MessageAnimatedEmoji {
                animated_emoji: tdlib_rs::types::AnimatedEmoji {
                    sticker: None,
                    sticker_width: 0,
                    sticker_height: 0,
                    fitzpatrick_type: 0,
                    sound: None,
                },
                emoji: "\u{2764}\u{FE0F}".into(),
            },
        );
        let mapped = map_content(&emoji);
        assert_eq!(mapped.text, "\u{2764}\u{FE0F}");
        assert_eq!(mapped.preview, "\u{2764}\u{FE0F}");
        assert!(mapped.photo.is_none() && mapped.video.is_none());

        let mut dice = tdlib_rs::types::MessageDice {
            initial_state: None,
            final_state: None,
            emoji: "\u{1F3B2}".into(),
            value: 0,
            success_animation_frame_number: 0,
        };
        let thrown = map_content(&tdlib_rs::enums::MessageContent::MessageDice(dice.clone()));
        assert_eq!(thrown.text, "\u{1F3B2}");
        dice.value = 5;
        let done = map_content(&tdlib_rs::enums::MessageContent::MessageDice(dice));
        assert_eq!(done.text, "\u{1F3B2} 5");
    }

    fn document(
        name: &str,
        mime: &str,
        id: i32,
        size: i64,
        mini: Option<tdlib_rs::types::Minithumbnail>,
    ) -> tdlib_rs::types::MessageDocument {
        tdlib_rs::types::MessageDocument {
            document: tdlib_rs::types::Document {
                file_name: name.into(),
                mime_type: mime.into(),
                minithumbnail: mini,
                thumbnail: None,
                document: tdlib_rs::types::File {
                    id,
                    size,
                    ..tdlib_rs::types::File::default()
                },
            },
            caption: tdlib_rs::types::FormattedText::default(),
        }
    }

    #[test]
    fn a_pdf_keeps_its_preview_and_a_plain_file_stays_downloadable() {
        let pdf = document(
            "notes.pdf",
            "application/pdf",
            11,
            2048,
            Some(tdlib_rs::types::Minithumbnail {
                width: 40,
                height: 52,
                data: "/9j/".into(),
            }),
        );
        let mapped = map_content(&tdlib_rs::enums::MessageContent::MessageDocument(pdf));
        assert!(mapped.text.is_empty());
        assert_eq!(mapped.preview, "notes.pdf");
        let file = mapped.file.expect("document");
        assert_eq!(file.name(), "notes.pdf");
        assert_eq!(file.kind_label(), "PDF");
        assert_eq!(file.size(), 2048);
        assert_eq!(file.file_id(), 11);
        assert!(file.file().is_none());
        let preview = file.preview().expect("pdf preview");
        assert!(preview.mini.is_some());
        assert_eq!((preview.width, preview.height), (40, 52));
        assert!(!format!("{file:?}").contains("/9j/"));

        let mut plain = document("notes.txt", "text/plain", 12, 0, None);
        plain.caption.text = "the notes".into();
        let mapped = map_content(&tdlib_rs::enums::MessageContent::MessageDocument(plain));
        assert_eq!(mapped.text, "the notes");
        assert_eq!(mapped.preview, "the notes");
        let file = mapped.file.expect("document");
        assert!(file.preview().is_none());
        assert_eq!(file.name(), "notes.txt");
        assert_eq!(file.file_id(), 12);
        assert_eq!(file.kind_label(), "TXT");
    }

    /// A call is not "Message": the row says what it was.
    #[test]
    fn a_call_has_a_label_with_its_end_and_length() {
        use tdlib_rs::enums::CallDiscardReason;
        let call = |is_video, discard_reason, duration| {
            map_content(&tdlib_rs::enums::MessageContent::MessageCall(
                tdlib_rs::types::MessageCall {
                    is_video,
                    discard_reason,
                    duration,
                },
            ))
            .text
        };
        assert_eq!(call(false, CallDiscardReason::HungUp, 75), "Call, 1:15");
        assert_eq!(
            call(true, CallDiscardReason::Missed, 0),
            "Video call, not answered"
        );
        assert_eq!(
            call(false, CallDiscardReason::Declined, 0),
            "Call, declined"
        );
        assert_eq!(call(false, CallDiscardReason::Empty, 0), "Call");
    }
}
