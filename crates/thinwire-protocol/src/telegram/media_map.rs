//! Turn a TDLib message into a caption, links, and a photo.
//!
//! Offsets from TDLib are UTF-16. [`crate::links_in`] stores UTF-8 indexes.

use crate::media::{MessageLink, MessagePhoto, PhotoSize, TextKind, TextSpan, links_in};
use crate::{ImageBytes, ImagePath};

pub(super) struct Mapped {
    pub text: String,
    pub preview: String,
    pub links: Vec<MessageLink>,
    pub photo: Option<MessagePhoto>,
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
                scan,
            }
        }
        tdlib_rs::enums::MessageContent::MessageSticker(_) => label("Sticker"),
        tdlib_rs::enums::MessageContent::MessageVideo(_) => label("Video"),
        tdlib_rs::enums::MessageContent::MessageAnimation(_) => label("Animation"),
        tdlib_rs::enums::MessageContent::MessageVoiceNote(_) => label("Voice message"),
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
        scan: false,
    }
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
    use super::thumbnail_bytes;

    #[test]
    fn a_minithumbnail_is_base64_jpeg_bytes() {
        let bytes = thumbnail_bytes("/9j/").expect("jpeg");
        assert_eq!(bytes, vec![0xff, 0xd8, 0xff]);
        assert!(thumbnail_bytes("").is_none());
        assert!(thumbnail_bytes("!!!!").is_none());
    }
}
