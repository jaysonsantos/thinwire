//! Links and photos on a chat message.
//!
//! Offsets in [`MessageLink`] are UTF-8 byte indexes into the message body.
//! TDLib reports entity offsets in UTF-16 code units. [`links_in`] converts
//! them. `Debug` prints no URL, path, or image bytes.

use std::fmt;

/// A link inside a message body.
#[derive(Clone, PartialEq, Eq)]
pub struct MessageLink {
    /// First byte of the visible label in the body.
    pub start: usize,
    /// Byte after the last byte of the label.
    pub end: usize,
    /// The address to open. An email uses `mailto:`.
    pub url: String,
}

impl fmt::Debug for MessageLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MessageLink({}..{})", self.start, self.end)
    }
}

/// One encoded image. `Debug` prints the length only.
#[derive(Clone, PartialEq, Eq)]
pub struct ImageBytes(Vec<u8>);

impl ImageBytes {
    #[must_use]
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for ImageBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ImageBytes({})", self.0.len())
    }
}

/// A local file path. `Debug` prints no path.
#[derive(Clone, PartialEq, Eq)]
pub struct ImagePath(String);

impl ImagePath {
    #[must_use]
    pub fn new(path: impl Into<String>) -> Self {
        Self(path.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ImagePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ImagePath")
    }
}

/// One size of a photo. The UI picks the size near twice the display width.
#[derive(Clone, PartialEq, Eq)]
pub struct PhotoSize {
    pub width: u32,
    pub height: u32,
    /// TDLib file id. `None` when [`Self::bytes`] already holds the image.
    pub file_id: Option<i32>,
    /// Set when TDLib has the file and the photo is not secret.
    pub path: Option<ImagePath>,
    /// Encoded bytes. A demo, a test, or a secret photo after download.
    pub bytes: Option<ImageBytes>,
}

impl fmt::Debug for PhotoSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PhotoSize")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("file_id", &self.file_id)
            .field("path", &self.path)
            .field("bytes", &self.bytes)
            .finish()
    }
}

impl PhotoSize {
    /// The image is already here: bytes, or a file that is not secret.
    #[must_use]
    pub fn ready(&self) -> bool {
        self.bytes.is_some() || self.path.is_some()
    }
}

/// A photo attached to a message. The message body is the caption.
#[derive(Clone, PartialEq, Eq)]
pub struct MessagePhoto {
    /// Pixel size of the photo, from the largest size TDLib sent.
    pub width: u32,
    pub height: u32,
    /// Tiny JPEG (or PNG in a demo). Drawn blurred at the final size.
    pub mini: Option<ImageBytes>,
    pub sizes: Vec<PhotoSize>,
    pub has_spoiler: bool,
    /// Do not keep a file, and do not open the full view.
    pub is_secret: bool,
    /// The inline download failed.
    pub failed: bool,
}

impl fmt::Debug for MessagePhoto {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MessagePhoto")
            .field("width", &self.width)
            .field("height", &self.height)
            .field(
                "mini",
                &self
                    .mini
                    .as_ref()
                    .map(ImageBytes::as_slice)
                    .map(<[u8]>::len),
            )
            .field("sizes", &self.sizes)
            .field("has_spoiler", &self.has_spoiler)
            .field("is_secret", &self.is_secret)
            .field("failed", &self.failed)
            .finish()
    }
}

/// One TDLib text entity, already filtered to a link kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextSpan {
    /// UTF-16 offset into the text.
    pub offset: i32,
    /// UTF-16 length.
    pub length: i32,
    pub kind: TextKind,
}

/// The link kinds the thread highlights.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextKind {
    /// The label is the URL.
    Url,
    /// The label and the target can differ.
    TextUrl(String),
    /// The label is an email address.
    Email,
}

/// Links for `text` from TDLib entities. A bad offset is left out.
#[must_use]
pub fn links_in(text: &str, spans: &[TextSpan]) -> Vec<MessageLink> {
    let mut links = Vec::new();
    for span in spans {
        let Some((start, end)) = utf16_range(text, span.offset, span.length) else {
            continue;
        };
        let label = &text[start..end];
        let Some(url) = link_url(label, &span.kind) else {
            continue;
        };
        if url.is_empty() {
            continue;
        }
        links.push(MessageLink { start, end, url });
    }
    links.sort_by_key(|link| (link.start, link.end));
    links
}

fn link_url(label: &str, kind: &TextKind) -> Option<String> {
    match kind {
        TextKind::Url => Some(with_scheme(label)),
        TextKind::TextUrl(url) => {
            let url = url.trim();
            if url.is_empty() {
                None
            } else {
                Some(url.to_string())
            }
        }
        TextKind::Email => {
            let address = label.trim();
            if address.is_empty() || address.contains(char::is_whitespace) {
                None
            } else if address.to_ascii_lowercase().starts_with("mailto:") {
                Some(address.to_string())
            } else {
                Some(format!("mailto:{address}"))
            }
        }
    }
}

/// Add `https://` when the text has no scheme.
#[must_use]
pub fn with_scheme(text: &str) -> String {
    let text = text.trim();
    if text.contains("://") || text.to_ascii_lowercase().starts_with("mailto:") {
        text.to_string()
    } else {
        format!("https://{text}")
    }
}

/// Map a UTF-16 range to a UTF-8 byte range. `None` when the range is not on
/// character boundaries or does not fit in `text`.
#[must_use]
pub fn utf16_range(text: &str, offset: i32, length: i32) -> Option<(usize, usize)> {
    let offset = usize::try_from(offset).ok()?;
    let length = usize::try_from(length).ok()?;
    if length == 0 {
        return None;
    }
    let start = utf16_to_byte(text, offset)?;
    let end = utf16_to_byte(text, offset + length)?;
    (start < end).then_some((start, end))
}

fn utf16_to_byte(text: &str, units: usize) -> Option<usize> {
    let mut seen = 0usize;
    for (byte, ch) in text.char_indices() {
        if seen == units {
            return Some(byte);
        }
        if seen > units {
            return None;
        }
        seen += ch.len_utf16();
    }
    (seen == units).then_some(text.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_offsets_skip_emoji_and_keep_ascii() {
        let text = "hi 👍 https://a.test";
        // "hi " is 3 units, the emoji is 2, the space is 1. The URL starts at 6.
        let url_at = utf16_range(text, 6, 14).expect("url");
        assert_eq!(&text[url_at.0..url_at.1], "https://a.test");
        assert!(utf16_range(text, 3, 1).is_none(), "inside the emoji");
        assert!(utf16_range(text, -1, 2).is_none());
        assert!(utf16_range(text, 0, 0).is_none());
    }

    #[test]
    fn links_keep_url_text_url_and_email_only() {
        let text = "see https://a.test and docs and ada@b.test";
        let https = text.find("https://a.test").expect("url") as i32;
        // The finder is a byte index. This string is ASCII, so it matches UTF-16.
        let docs = text.find("docs").expect("docs") as i32;
        let mail = text.find("ada@b.test").expect("mail") as i32;
        let links = links_in(
            text,
            &[
                TextSpan {
                    offset: https,
                    length: "https://a.test".len() as i32,
                    kind: TextKind::Url,
                },
                TextSpan {
                    offset: docs,
                    length: 4,
                    kind: TextKind::TextUrl("https://docs.example/guide".into()),
                },
                TextSpan {
                    offset: mail,
                    length: "ada@b.test".len() as i32,
                    kind: TextKind::Email,
                },
            ],
        );
        assert_eq!(links.len(), 3);
        assert_eq!(links[0].url, "https://a.test");
        assert_eq!(links[1].url, "https://docs.example/guide");
        assert_eq!(&text[links[1].start..links[1].end], "docs");
        assert_eq!(links[2].url, "mailto:ada@b.test");
    }

    #[test]
    fn a_bare_host_gets_https_and_an_empty_target_is_dropped() {
        assert_eq!(with_scheme("a.test/x"), "https://a.test/x");
        assert_eq!(with_scheme("http://a.test"), "http://a.test");
        let links = links_in(
            "docs",
            &[TextSpan {
                offset: 0,
                length: 4,
                kind: TextKind::TextUrl("  ".into()),
            }],
        );
        assert!(links.is_empty());
    }

    #[test]
    fn debug_hides_urls_paths_and_bytes() {
        let link = MessageLink {
            start: 0,
            end: 3,
            url: "https://hidden.example/a".into(),
        };
        let photo = MessagePhoto {
            width: 2,
            height: 2,
            mini: Some(ImageBytes::new(vec![1, 2, 3])),
            sizes: vec![PhotoSize {
                width: 2,
                height: 2,
                file_id: Some(7),
                path: Some(ImagePath::new("/tmp/hidden-photo")),
                bytes: Some(ImageBytes::new(b"jpeg".to_vec())),
            }],
            has_spoiler: false,
            is_secret: true,
            failed: false,
        };
        let link_debug = format!("{link:?}");
        let photo_debug = format!("{photo:?}");
        assert!(!link_debug.contains("hidden.example"));
        assert!(!photo_debug.contains("hidden-photo"));
        assert!(!photo_debug.contains("jpeg"));
        assert!(photo_debug.contains("is_secret"));
        assert!(photo_debug.contains("ImageBytes"));
    }
}
