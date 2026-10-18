//! Links and photos on a chat message.
//!
//! Offsets in [`MessageLink`] are UTF-8 byte indexes into the message body.
//! TDLib reports entity offsets in UTF-16 code units. [`links_in`] converts
//! them. A link stays only when [`openable_url`] accepts the scheme.
//! `Debug` prints no URL, path, or image bytes.

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

/// Links for `text` from TDLib entities.
/// A bad offset or a blocked scheme is left out.
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
        links.push(MessageLink { start, end, url });
    }
    links.sort_by_key(|link| (link.start, link.end));
    links
}

fn link_url(label: &str, kind: &TextKind) -> Option<String> {
    match kind {
        TextKind::Url => openable_url(label),
        TextKind::TextUrl(url) => openable_url(url),
        TextKind::Email => mailto_url(label),
    }
}

fn mailto_url(label: &str) -> Option<String> {
    let address = label.trim();
    if address.is_empty() || address.contains(char::is_whitespace) {
        return None;
    }
    if address.to_ascii_lowercase().starts_with("mailto:") {
        openable_url(address)
    } else {
        openable_url(&format!("mailto:{address}"))
    }
}

/// Add `https://` when the text has no scheme.
///
/// Call [`openable_url`] before you keep or open an address.
#[must_use]
pub fn with_scheme(text: &str) -> String {
    let text = text.trim();
    if text.contains("://") || text.to_ascii_lowercase().starts_with("mailto:") {
        text.to_string()
    } else {
        format!("https://{text}")
    }
}

/// The address to open, or `None` when the scheme is not allowed.
///
/// The allowed schemes are `http`, `https`, and `mailto`. A text with no
/// scheme gets `https://`. `example.com:443` is a host and a port, not a
/// scheme. `tg` is not allowed: that address starts a program on the
/// computer. An `https://t.me` address uses `https` and stays.
#[must_use]
pub fn openable_url(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if let Some(scheme) = explicit_scheme(raw) {
        return scheme_allowed(scheme).then(|| raw.to_string());
    }
    let url = with_scheme(raw);
    let scheme = explicit_scheme(&url)?;
    scheme_allowed(scheme).then_some(url)
}

fn scheme_allowed(scheme: &str) -> bool {
    const ALLOWED: [&str; 3] = ["http", "https", "mailto"];
    ALLOWED
        .iter()
        .any(|allowed| scheme.eq_ignore_ascii_case(allowed))
}

/// The scheme when `value` starts with `scheme:`.
///
/// `localhost:8080` and `example.com:443` have no scheme. The part before
/// `:` is a host, and the part after `:` is a port. `javascript:1` is a
/// scheme: `javascript` is not a host.
fn explicit_scheme(value: &str) -> Option<&str> {
    let bytes = value.as_bytes();
    if bytes.first().is_none_or(|byte| !byte.is_ascii_alphabetic()) {
        return None;
    }
    let mut end = 1usize;
    while end < bytes.len() && is_scheme_byte(bytes[end]) {
        end += 1;
    }
    if end >= bytes.len() || bytes[end] != b':' {
        return None;
    }
    let scheme = &value[..end];
    let rest = &value[end + 1..];
    if rest_is_port(rest) && looks_like_host(scheme) {
        return None;
    }
    Some(scheme)
}

fn is_scheme_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'-' || byte == b'.'
}

fn looks_like_host(scheme: &str) -> bool {
    scheme.contains('.') || scheme.eq_ignore_ascii_case("localhost")
}

fn rest_is_port(rest: &str) -> bool {
    if rest.is_empty() || rest.starts_with("//") {
        return false;
    }
    let port = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit())
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
        assert_eq!(
            openable_url("a.test/x").as_deref(),
            Some("https://a.test/x")
        );
        assert_eq!(
            openable_url("http://a.test").as_deref(),
            Some("http://a.test")
        );
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
    fn a_blocked_scheme_is_not_a_link_and_does_not_open() {
        // `with_scheme` returns the file address unchanged.
        // `openable_url` rejects that address, and `links_in` drops the link.
        assert_eq!(with_scheme("file:///etc/passwd"), "file:///etc/passwd");
        let blocked = [
            "file:///etc/passwd",
            "FILE:///tmp/x",
            "javascript:alert(1)",
            "JavaScript:alert(1)",
            "javascript:1",
            "custom://open",
            "smb://server/share",
            "ms-msdt:id",
            "search-ms:query=docs",
            "vscode://file/tmp/x",
            "tg://resolve?domain=thinwire",
        ];
        for target in blocked {
            assert!(openable_url(target).is_none(), "{target}");
            let as_text = links_in(
                "see the doc",
                &[TextSpan {
                    offset: 0,
                    length: "see the doc".len() as i32,
                    kind: TextKind::TextUrl(target.into()),
                }],
            );
            assert!(as_text.is_empty(), "{target}");
            let as_url = links_in(
                target,
                &[TextSpan {
                    offset: 0,
                    length: target.len() as i32,
                    kind: TextKind::Url,
                }],
            );
            assert!(as_url.is_empty(), "{target}");
        }
    }

    #[test]
    fn http_https_and_mailto_stay_links_beside_a_blocked_scheme() {
        let text = "see the doc and https://a.test";
        let docs = text.find("see the doc").expect("label") as i32;
        let https = text.find("https://a.test").expect("url") as i32;
        let links = links_in(
            text,
            &[
                TextSpan {
                    offset: docs,
                    length: "see the doc".len() as i32,
                    kind: TextKind::TextUrl("file:///etc/passwd".into()),
                },
                TextSpan {
                    offset: https,
                    length: "https://a.test".len() as i32,
                    kind: TextKind::Url,
                },
            ],
        );
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].url, "https://a.test");
        assert_eq!(&text[links[0].start..links[0].end], "https://a.test");
        assert_eq!(
            openable_url(" HTTPS://a.test/x ").as_deref(),
            Some("HTTPS://a.test/x")
        );
        assert_eq!(
            openable_url("localhost:8080").as_deref(),
            Some("https://localhost:8080")
        );
        assert_eq!(
            openable_url("a.test:443/x").as_deref(),
            Some("https://a.test:443/x")
        );
        let mail = links_in(
            "ada@b.test",
            &[TextSpan {
                offset: 0,
                length: "ada@b.test".len() as i32,
                kind: TextKind::Email,
            }],
        );
        assert_eq!(mail.len(), 1);
        assert_eq!(mail[0].url, "mailto:ada@b.test");
        assert_eq!(
            openable_url("https://t.me/thinwire").as_deref(),
            Some("https://t.me/thinwire")
        );
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
