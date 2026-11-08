//! Links, photos, voice notes, and documents on a chat message.
//!
//! Offsets in [`MessageLink`] are UTF-8 byte indexes into the message body.
//! TDLib reports entity offsets in UTF-16 code units. [`links_in`] converts
//! them. A link stays only when [`openable_url`] accepts the scheme.
//! `Debug` prints no URL, path, or image bytes.

use std::fmt;
use std::path::{Path, PathBuf};

/// A link inside a message body: a web address, or a mention of a person.
#[derive(Clone, PartialEq, Eq)]
pub struct MessageLink {
    /// First byte of the visible label in the body.
    pub start: usize,
    /// Byte after the last byte of the label.
    pub end: usize,
    /// What a click opens.
    pub target: LinkTarget,
}

/// What a click on a [`MessageLink`] opens.
#[derive(Clone, PartialEq, Eq)]
pub enum LinkTarget {
    /// The address to open. An email uses `mailto:`.
    Url(String),
    /// A mention of a person. The label is "@" and the name. The value is
    /// the conversation id of the one-to-one chat with that person, when the
    /// chat list has that chat. `None` draws the mention with no click.
    Chat(Option<String>),
}

impl MessageLink {
    /// A link to a web address or a `mailto:` address.
    #[must_use]
    pub fn web(start: usize, end: usize, url: impl Into<String>) -> Self {
        Self {
            start,
            end,
            target: LinkTarget::Url(url.into()),
        }
    }

    /// A mention of a person, with the chat that a click opens.
    #[must_use]
    pub const fn mention(start: usize, end: usize, chat: Option<String>) -> Self {
        Self {
            start,
            end,
            target: LinkTarget::Chat(chat),
        }
    }

    /// The web address, when this link is one.
    #[must_use]
    pub fn url(&self) -> Option<&str> {
        match &self.target {
            LinkTarget::Url(url) => Some(url),
            LinkTarget::Chat(_) => None,
        }
    }

    /// The chat that a click on this mention opens.
    #[must_use]
    pub fn chat(&self) -> Option<&str> {
        match &self.target {
            LinkTarget::Chat(chat) => chat.as_deref(),
            LinkTarget::Url(_) => None,
        }
    }

    /// This link is a mention of a person.
    #[must_use]
    pub const fn is_mention(&self) -> bool {
        matches!(self.target, LinkTarget::Chat(_))
    }
}

impl fmt::Debug for MessageLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = if self.is_mention() {
            "Mention"
        } else {
            "MessageLink"
        };
        write!(f, "{kind}({}..{})", self.start, self.end)
    }
}

/// Mention links for `body` from byte ranges. A range that is empty, out of
/// order, overlaps the one before, leaves the body, or does not start with
/// "@" on a char boundary is left out.
#[must_use]
pub fn mention_links(
    body: &str,
    mentions: Vec<(usize, usize, Option<String>)>,
) -> Vec<MessageLink> {
    let mut links = Vec::new();
    let mut cursor = 0usize;
    for (start, end, chat) in mentions {
        let valid = start >= cursor
            && start < end
            && end <= body.len()
            && body.is_char_boundary(start)
            && body.is_char_boundary(end)
            && body[start..].starts_with('@');
        if !valid {
            continue;
        }
        links.push(MessageLink::mention(start, end, chat));
        cursor = end;
    }
    links
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

/// A local voice file. `Debug` prints `AudioPath` and no path.
#[derive(Clone, PartialEq, Eq)]
pub struct AudioPath(String);

impl AudioPath {
    /// Empty is refused.
    #[must_use]
    pub fn new(path: impl Into<String>) -> Option<Self> {
        let path = path.into();
        (!path.is_empty()).then_some(Self(path))
    }

    #[must_use]
    pub fn as_path(&self) -> &Path {
        Path::new(&self.0)
    }
}

impl fmt::Debug for AudioPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AudioPath")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoiceStop {
    /// Missing bytes, a bad host, a network error, or a container the player rejected.
    Unplayable,
    SlackFilesDenied,
}

/// One voice note or audio file the user can play. `file_id` names the
/// download for its adapter: a TDLib file, or a handle of a helper or an
/// in-app adapter for this run.
#[derive(Clone, PartialEq, Eq)]
pub struct MessageAudio {
    duration_secs: u32,
    file_id: i32,
    file: Option<AudioPath>,
    stop: Option<VoiceStop>,
}

impl MessageAudio {
    /// `None` when `file_id` is not positive. A negative duration becomes 0.
    #[must_use]
    pub fn telegram(duration_secs: i32, file_id: i32, file: Option<AudioPath>) -> Option<Self> {
        Self::new(u32::try_from(duration_secs).unwrap_or(0), file_id, file)
    }

    /// `None` when `file_id` is not positive. A zero duration means that the
    /// protocol does not say.
    #[must_use]
    pub fn new(duration_secs: u32, file_id: i32, file: Option<AudioPath>) -> Option<Self> {
        if file_id <= 0 {
            return None;
        }
        Some(Self {
            duration_secs,
            file_id,
            file,
            stop: None,
        })
    }

    #[must_use]
    pub fn duration_secs(&self) -> u32 {
        self.duration_secs
    }

    #[must_use]
    pub fn file_id(&self) -> i32 {
        self.file_id
    }

    #[must_use]
    pub fn file(&self) -> Option<&AudioPath> {
        self.file.as_ref()
    }

    #[must_use]
    pub fn failed(&self) -> bool {
        self.stop.is_some()
    }

    #[must_use]
    pub fn stop(&self) -> Option<VoiceStop> {
        self.stop
    }

    /// Stores `path` and clears the stop. A finished download is playable.
    pub fn set_file(&mut self, path: AudioPath) {
        self.file = Some(path);
        self.stop = None;
    }

    pub fn mark_failed(&mut self) {
        self.stop = Some(VoiceStop::Unplayable);
    }

    pub fn mark_stopped(&mut self, stop: VoiceStop) {
        self.stop = Some(stop);
    }

    pub fn clear_failed(&mut self) {
        self.stop = None;
    }
}

impl fmt::Debug for MessageAudio {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MessageAudio")
            .field("duration_secs", &self.duration_secs)
            .field("file_id", &self.file_id)
            .field("file", &self.file)
            .field("stop", &self.stop)
            .finish()
    }
}

/// One video clip the user can play in the app (ADR 0015). `file_id` names
/// the download for its adapter, as [`MessageAudio`] does. The path is an
/// [`AudioPath`]: a local media file whose `Debug` shows no path.
#[derive(Clone, PartialEq, Eq)]
pub struct MessageVideo {
    duration_secs: u32,
    width: u32,
    height: u32,
    file_id: i32,
    file: Option<AudioPath>,
    failed: bool,
}

impl MessageVideo {
    /// `None` when `file_id` is not positive. A zero duration or size means
    /// that the protocol does not say.
    #[must_use]
    pub fn new(
        duration_secs: u32,
        width: u32,
        height: u32,
        file_id: i32,
        file: Option<AudioPath>,
    ) -> Option<Self> {
        (file_id > 0).then_some(Self {
            duration_secs,
            width,
            height,
            file_id,
            file,
            failed: false,
        })
    }

    #[must_use]
    pub fn duration_secs(&self) -> u32 {
        self.duration_secs
    }

    /// Pixel size from the protocol. Zero when it does not say.
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    #[must_use]
    pub fn file_id(&self) -> i32 {
        self.file_id
    }

    #[must_use]
    pub fn file(&self) -> Option<&AudioPath> {
        self.file.as_ref()
    }

    #[must_use]
    pub fn failed(&self) -> bool {
        self.failed
    }

    /// Stores `path` and clears `failed`.
    pub fn set_file(&mut self, path: AudioPath) {
        self.file = Some(path);
        self.failed = false;
    }

    pub fn mark_failed(&mut self) {
        self.failed = true;
    }

    pub fn clear_failed(&mut self) {
        self.failed = false;
    }
}

impl fmt::Debug for MessageVideo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MessageVideo")
            .field("duration_secs", &self.duration_secs)
            .field("size", &(self.width, self.height))
            .field("file_id", &self.file_id)
            .field("file", &self.file)
            .field("failed", &self.failed)
            .finish()
    }
}

/// A downloaded photo. It has a path, bytes, or both.
///
/// `finish_photo` sends `PhotoFailed` when the file has neither, so this
/// value is never empty.
#[derive(Clone, PartialEq, Eq)]
pub struct PhotoFile {
    path: Option<ImagePath>,
    bytes: Option<ImageBytes>,
}

impl PhotoFile {
    /// A file on disk. The path is the image.
    #[must_use]
    pub fn from_path(path: ImagePath) -> Self {
        Self {
            path: Some(path),
            bytes: None,
        }
    }

    /// Encoded bytes, for a secret photo. `None` when `bytes` is empty.
    #[must_use]
    pub fn from_bytes(bytes: ImageBytes) -> Option<Self> {
        if bytes.is_empty() {
            None
        } else {
            Some(Self {
                path: None,
                bytes: Some(bytes),
            })
        }
    }

    /// `None` when there is no path and no bytes.
    #[must_use]
    pub fn new(path: Option<ImagePath>, bytes: Option<ImageBytes>) -> Option<Self> {
        let bytes = bytes.filter(|bytes| !bytes.is_empty());
        if path.is_none() && bytes.is_none() {
            None
        } else {
            Some(Self { path, bytes })
        }
    }

    #[must_use]
    pub fn path(&self) -> Option<&ImagePath> {
        self.path.as_ref()
    }

    #[must_use]
    pub fn bytes(&self) -> Option<&ImageBytes> {
        self.bytes.as_ref()
    }
}

impl fmt::Debug for PhotoFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PhotoFile")
            .field("path", &self.path)
            .field("bytes", &self.bytes)
            .finish()
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

/// One document the user can download. `file_id` names the download for its
/// adapter, as [`MessageAudio`] does. The path is an [`AudioPath`]: a local
/// file whose `Debug` shows no path. [`Self::preview`] is a thumbnail the
/// protocol already sent. It is not a rendered page.
#[derive(Clone, PartialEq, Eq)]
pub struct MessageFile {
    name: String,
    mime: String,
    size: u64,
    file_id: i32,
    file: Option<AudioPath>,
    failed: bool,
    preview: Option<MessagePhoto>,
}

impl MessageFile {
    /// `name` is the file name, not a path. An empty name is shown as "File".
    /// `size` is bytes, or 0 when the protocol does not say. `file_id` may be
    /// 0 when the file cannot be downloaded.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        mime: impl Into<String>,
        size: u64,
        file_id: i32,
        file: Option<AudioPath>,
        preview: Option<MessagePhoto>,
    ) -> Self {
        Self {
            name: display_name(&name.into()),
            mime: mime.into(),
            size,
            file_id,
            file,
            failed: false,
            preview,
        }
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn mime(&self) -> &str {
        &self.mime
    }

    #[must_use]
    pub fn size(&self) -> u64 {
        self.size
    }

    #[must_use]
    pub fn kind_label(&self) -> String {
        if is_pdf(&self.name, &self.mime) {
            return "PDF".to_string();
        }
        if let Some(ext) = extension(&self.name)
            && (1..=5).contains(&ext.len())
            && ext.chars().all(|ch| ch.is_ascii_alphanumeric())
        {
            return ext.to_ascii_uppercase();
        }
        "File".to_string()
    }

    /// `None` when the protocol did not say.
    #[must_use]
    pub fn size_label(&self) -> Option<String> {
        size_label(self.size)
    }

    #[must_use]
    pub fn file_id(&self) -> i32 {
        self.file_id
    }

    #[must_use]
    pub fn file(&self) -> Option<&AudioPath> {
        self.file.as_ref()
    }

    #[must_use]
    pub fn failed(&self) -> bool {
        self.failed
    }

    /// A thumbnail or minithumbnail, when the protocol sent one.
    #[must_use]
    pub fn preview(&self) -> Option<&MessagePhoto> {
        self.preview.as_ref()
    }

    #[must_use]
    pub fn preview_mut(&mut self) -> Option<&mut MessagePhoto> {
        self.preview.as_mut()
    }

    pub fn set_file(&mut self, path: AudioPath) {
        self.file = Some(path);
        self.failed = false;
    }

    pub fn mark_failed(&mut self) {
        self.failed = true;
    }

    pub fn clear_failed(&mut self) {
        self.failed = false;
    }
}

impl fmt::Debug for MessageFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MessageFile")
            .field("name", &self.name)
            .field("mime", &self.mime)
            .field("size", &self.size)
            .field("file_id", &self.file_id)
            .field("file", &self.file)
            .field("failed", &self.failed)
            .field("preview", &self.preview)
            .finish()
    }
}

fn display_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name).trim();
    if base.is_empty() {
        "File".to_string()
    } else {
        base.to_string()
    }
}

fn extension(name: &str) -> Option<&str> {
    let ext = name.rsplit_once('.')?.1;
    (!ext.is_empty() && !ext.contains(['/', '\\'])).then_some(ext)
}

fn is_pdf(name: &str, mime: &str) -> bool {
    mime.eq_ignore_ascii_case("application/pdf")
        || extension(name).is_some_and(|ext| ext.eq_ignore_ascii_case("pdf"))
}

fn size_label(size: u64) -> Option<String> {
    if size == 0 {
        return None;
    }
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * 1024 * 1024;
    let (value, unit) = if size < KIB {
        return Some(format!("{size} B"));
    } else if size < MIB {
        (size.div_ceil(KIB), "KB")
    } else if size < GIB {
        (size.div_ceil(MIB), "MB")
    } else {
        (size.div_ceil(GIB), "GB")
    };
    Some(format!("{value} {unit}"))
}

/// Still chat photo for an inbox avatar, plus the accent used for initials.
///
/// Telegram fills [`Self::file_id`] from `chat.photo.small` (160×160).
/// Other protocols leave the photo empty. The shell draws initials from
/// [`Self::accent`].
#[derive(Clone, PartialEq, Eq, Default)]
pub struct ChatAvatar {
    /// TDLib file id of the small photo. Empty when the chat has no photo.
    pub file_id: Option<i32>,
    /// Local file after download. The UI reads it off the UI thread.
    pub path: Option<ImagePath>,
    /// Encoded bytes for a demo or a test.
    pub bytes: Option<ImageBytes>,
    /// `accent_color_id` folded into palette 0–6.
    pub accent: u8,
    /// The core has queued a download of [`Self::file_id`].
    /// The inbox asks once. A new file id clears it.
    pub requested: bool,
    /// The last download of [`Self::file_id`] failed.
    /// It stays failed until the file id changes. The next launch loads again.
    pub failed: bool,
}

impl ChatAvatar {
    /// Map a TDLib `accent_color_id` onto palette 0–6.
    /// An id outside that range uses color 0.
    #[must_use]
    pub const fn accent_index(accent_color_id: i32) -> u8 {
        if accent_color_id >= 0 && accent_color_id <= 6 {
            accent_color_id as u8
        } else {
            0
        }
    }

    /// The image is already here: bytes, or a downloaded file.
    #[must_use]
    pub fn ready(&self) -> bool {
        self.bytes.as_ref().is_some_and(|bytes| !bytes.is_empty()) || self.path.is_some()
    }
}

impl fmt::Debug for ChatAvatar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChatAvatar")
            .field("file_id", &self.file_id)
            .field("path", &self.path)
            .field("bytes", &self.bytes)
            .field("accent", &self.accent)
            .field("requested", &self.requested)
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
        links.push(MessageLink::web(start, end, url));
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
        assert_eq!(links[0].url(), Some("https://a.test"));
        assert_eq!(links[1].url(), Some("https://docs.example/guide"));
        assert_eq!(&text[links[1].start..links[1].end], "docs");
        assert_eq!(links[2].url(), Some("mailto:ada@b.test"));
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
        assert_eq!(links[0].url(), Some("https://a.test"));
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
        assert_eq!(mail[0].url(), Some("mailto:ada@b.test"));
        assert_eq!(
            openable_url("https://t.me/thinwire").as_deref(),
            Some("https://t.me/thinwire")
        );
    }

    #[test]
    fn a_mention_range_must_start_with_at_and_stay_in_the_body() {
        let body = "@Zoë hi @Bo";
        let zoe_end = "@Zoë".len();
        let links = mention_links(
            body,
            vec![
                (0, zoe_end, Some("whatsapp:1@s.whatsapp.net".into())),
                (0, 2, None),
                (zoe_end + 4, body.len(), None),
                (0, 3, None),
                (zoe_end + 4, body.len() + 1, None),
            ],
        );
        assert_eq!(
            links,
            vec![
                MessageLink::mention(0, zoe_end, Some("whatsapp:1@s.whatsapp.net".into())),
                MessageLink::mention(zoe_end + 4, body.len(), None),
            ]
        );
        assert!(links[0].is_mention() && links[0].url().is_none());
        assert_eq!(format!("{:?}", links[0]), format!("Mention(0..{zoe_end})"));
        assert!(
            mention_links(body, vec![(2, 4, None)]).is_empty(),
            "inside a char"
        );
    }

    #[test]
    fn debug_hides_urls_paths_and_bytes() {
        let link = MessageLink::web(0, 3, "https://hidden.example/a");
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

    #[test]
    fn a_voice_note_keeps_a_path_and_debug_hides_it() {
        assert!(AudioPath::new("").is_none());
        assert!(MessageAudio::telegram(3, 0, None).is_none());
        assert!(MessageAudio::telegram(3, -1, None).is_none());
        let path = AudioPath::new("/tmp/hidden-voice.ogg").expect("path");
        assert_eq!(
            path.as_path(),
            std::path::Path::new("/tmp/hidden-voice.ogg")
        );
        let mut audio = MessageAudio::telegram(-4, 42, None).expect("note");
        assert_eq!(audio.duration_secs(), 0);
        assert_eq!(audio.file_id(), 42);
        assert!(audio.file().is_none());
        assert!(!audio.failed());
        audio.mark_failed();
        assert!(audio.failed());
        assert_eq!(audio.stop(), Some(VoiceStop::Unplayable));
        audio.set_file(path);
        assert!(!audio.failed());
        assert!(audio.file().is_some());
        let debug = format!("{audio:?}");
        assert!(!debug.contains("hidden-voice"));
        assert!(debug.contains("AudioPath"));
        audio.mark_stopped(VoiceStop::SlackFilesDenied);
        assert_eq!(audio.stop(), Some(VoiceStop::SlackFilesDenied));
        assert!(audio.file().is_some());
        audio.clear_failed();
        assert!(!audio.failed());
        audio.mark_failed();
        audio.set_file(AudioPath::new("/tmp/hidden-voice.ogg").expect("path"));
        assert!(audio.stop().is_none());
    }
}

/// The kind of a file that the user attaches to a message. Only photos and
/// videos go out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaKind {
    Photo,
    Video,
}

/// Files that one send takes at most. A protocol that takes fewer in one
/// message sends more than one message.
pub const MAX_ATTACHMENTS: usize = 30;

/// Files in one Telegram album or one Discord message.
pub const FILES_PER_MESSAGE: usize = 10;

/// The largest file that a send takes, in bytes.
pub const MAX_ATTACHMENT_BYTES: u64 = 100 * 1024 * 1024;

impl MediaKind {
    /// The kind of a file from its extension. `None` for another file.
    #[must_use]
    pub fn of_path(path: &Path) -> Option<Self> {
        let extension = path.extension()?.to_str()?.to_ascii_lowercase();
        match extension.as_str() {
            "jpg" | "jpeg" | "png" | "webp" | "gif" | "heic" | "heif" | "bmp" => Some(Self::Photo),
            "mp4" | "m4v" | "mov" | "webm" | "mkv" | "avi" | "3gp" => Some(Self::Video),
            _ => None,
        }
    }
}

/// One file that the user attached to a message. `Debug` shows the kind
/// only: a path can hold a user name or a file name.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct OutgoingFile {
    pub path: PathBuf,
    pub kind: MediaKind,
}

impl OutgoingFile {
    /// `None` when the file is not a photo or a video.
    #[must_use]
    pub fn new(path: PathBuf) -> Option<Self> {
        let kind = MediaKind::of_path(&path)?;
        Some(Self { path, kind })
    }

    /// The file name, for the upload and for the tray of the compose field.
    #[must_use]
    pub fn file_name(&self) -> String {
        self.path
            .file_name()
            .and_then(|name| name.to_str())
            .map_or_else(|| "file".to_owned(), str::to_owned)
    }

    /// The MIME type from the extension.
    #[must_use]
    pub fn mime(&self) -> &'static str {
        let extension = self
            .path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        match extension.as_str() {
            "jpg" | "jpeg" => "image/jpeg",
            "png" => "image/png",
            "webp" => "image/webp",
            "gif" => "image/gif",
            "heic" => "image/heic",
            "heif" => "image/heif",
            "bmp" => "image/bmp",
            "mp4" | "m4v" => "video/mp4",
            "mov" => "video/quicktime",
            "webm" => "video/webm",
            "mkv" => "video/x-matroska",
            "avi" => "video/x-msvideo",
            "3gp" => "video/3gpp",
            _ => "application/octet-stream",
        }
    }
}

impl fmt::Debug for OutgoingFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "OutgoingFile({:?})", self.kind)
    }
}

#[cfg(test)]
mod outgoing_tests {
    use super::*;

    #[test]
    fn only_photos_and_videos_attach() {
        assert_eq!(
            OutgoingFile::new("/home/a/IMG_1.JPG".into()).map(|file| file.kind),
            Some(MediaKind::Photo)
        );
        assert_eq!(
            OutgoingFile::new("clip.mov".into()).map(|file| file.mime()),
            Some("video/quicktime")
        );
        assert!(OutgoingFile::new("notes.pdf".into()).is_none());
        assert!(OutgoingFile::new("no-extension".into()).is_none());
        let file = OutgoingFile::new("/home/secret-user/cat.png".into()).expect("photo");
        assert_eq!(file.file_name(), "cat.png");
        assert!(!format!("{file:?}").contains("secret-user"));
    }

    #[test]
    fn a_document_shows_its_name_and_not_a_directory() {
        let file = MessageFile::new(
            "/home/secret-user/notes.pdf",
            "application/pdf",
            2048,
            11,
            None,
            None,
        );
        assert_eq!(file.name(), "notes.pdf");
        assert_eq!(file.kind_label(), "PDF");
        assert_eq!(file.size_label().as_deref(), Some("2 KB"));
        assert!(!format!("{file:?}").contains("secret-user"));
        let plain = MessageFile::new("notes.txt", "text/plain", 0, 12, None, None);
        assert!(plain.preview().is_none());
        assert_eq!(plain.kind_label(), "TXT");
        assert!(plain.size_label().is_none());
    }
}
