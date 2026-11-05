//! Turn a TDLib message into a caption, links, a photo, and a voice note.
//!
//! Offsets from TDLib are UTF-16. [`crate::links_in`] stores UTF-8 indexes.

use crate::media::{
    AudioPath, MessageAudio, MessageLink, MessagePhoto, PhotoSize, TextKind, TextSpan, links_in,
};
use crate::{ImageBytes, ImagePath};

pub(super) struct Mapped {
    pub text: String,
    pub preview: String,
    pub links: Vec<MessageLink>,
    pub photo: Option<MessagePhoto>,
    pub audio: Option<MessageAudio>,
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
                scan,
            }
        }
        tdlib_rs::enums::MessageContent::MessageSticker(_) => label("Sticker"),
        tdlib_rs::enums::MessageContent::MessageVideo(_) => label("Video"),
        tdlib_rs::enums::MessageContent::MessageAnimation(_) => label("Animation"),
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
        scan: false,
    }
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
    fn a_voice_note_is_playable_and_other_audio_stays_a_label() {
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
        let music = map_content(&music);
        assert!(music.audio.is_none());
        assert_eq!(music.preview, "Message");

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
        let note = map_content(&note);
        assert!(note.audio.is_none());
        assert_eq!(note.preview, "Message");

        let expired = map_content(&tdlib_rs::enums::MessageContent::MessageExpiredVoiceNote);
        assert!(expired.audio.is_none());
        assert_eq!(expired.preview, "Message");
    }
}
