//! Turn a TDLib message into a caption, links, a photo, and a voice note.
//!
//! Offsets from TDLib are UTF-16. [`crate::links_in`] stores UTF-8 indexes.

use crate::media::{
    AudioPath, MessageAudio, MessageLink, MessagePhoto, MessageVideo, PhotoSize, TextKind,
    TextSpan, links_in,
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
                scan,
            }
        }
        tdlib_rs::enums::MessageContent::MessageSticker(_) => label("Sticker"),
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
            },
            None,
        ),
        tdlib_rs::enums::MessageContent::MessageAudio(message) => map_music(message),
        // This TDLib build has no `is_secret` on a voice note. An expired
        // voice note is `MessageExpiredVoiceNote` and stays on the label arm.
        tdlib_rs::enums::MessageContent::MessageVoiceNote(note) => map_voice(note),
        tdlib_rs::enums::MessageContent::MessageDocument(_) => label("File"),
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
        scan: false,
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
        .filter(|thumbnail| {
            matches!(
                thumbnail.format,
                tdlib_rs::enums::ThumbnailFormat::Jpeg
                    | tdlib_rs::enums::ThumbnailFormat::Png
                    | tdlib_rs::enums::ThumbnailFormat::Webp
            )
        })
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
    mapped.video = MessageVideo::new(size(clip.duration), width, height, clip.file.id, local);
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
}
