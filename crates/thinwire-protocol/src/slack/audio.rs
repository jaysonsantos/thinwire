//! Which Slack file can play, and what a download body means.
//!
//! The session passes plain files, then a status and a body. This module
//! does not call Slack and does not see a token.

use std::fmt;
use std::hash::{BuildHasher, BuildHasherDefault, DefaultHasher};

use crate::voice_files::VOICE_MAX_BYTES;

/// `https` address on `files.slack.com`. The string stays private.
#[derive(Clone, PartialEq, Eq)]
pub struct FilesHostUrl(String);

impl FilesHostUrl {
    /// `https` and host `files.slack.com`, with an optional numeric port.
    ///
    /// A host that only contains that name is a different site. Userinfo,
    /// whitespace, a backslash, or `%` in the host is rejected, so the bot
    /// token is not sent there.
    pub(super) fn parse(raw: &str) -> Option<Self> {
        if raw.chars().any(char::is_whitespace) || raw.contains('\\') {
            return None;
        }
        let rest = raw.strip_prefix("https://")?;
        let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let authority = &rest[..authority_end];
        if authority.is_empty() || authority.contains('@') || authority.contains('%') {
            return None;
        }
        let host = match authority.rsplit_once(':') {
            Some((host, port)) => {
                if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
                    return None;
                }
                host
            }
            None => authority,
        };
        host.eq_ignore_ascii_case("files.slack.com")
            .then(|| Self(raw.to_string()))
    }

    /// The GET target. Only the download task reads it.
    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for FilesHostUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FilesHostUrl")
    }
}

impl fmt::Display for FilesHostUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FilesHostUrl")
    }
}

/// One Slack file after the library type is gone.
#[derive(Clone, PartialEq, Eq)]
pub struct IncomingFile {
    pub mimetype: String,
    pub filetype: String,
    pub name: String,
    pub(super) url: Option<FilesHostUrl>,
}

impl fmt::Debug for IncomingFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IncomingFile")
            .field("mimetype", &self.mimetype)
            .field("filetype", &self.filetype)
            .field("name", &self.name)
            .finish()
    }
}

/// The one file the player asks for.
#[derive(Clone, PartialEq, Eq)]
pub(super) struct Clip {
    pub handle: i32,
    pub url: FilesHostUrl,
}

/// Body to show, and the clip when the first playable file has an address.
#[derive(Clone, PartialEq, Eq)]
pub(super) struct Shown {
    pub body: String,
    pub clip: Option<Clip>,
}

/// What the download body is. `Debug` prints a length, not the bytes.
#[derive(Clone, PartialEq, Eq)]
pub(super) enum FetchClass {
    Audio(Vec<u8>),
    FilesUnread,
    Miss,
}

impl fmt::Debug for FetchClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Audio(bytes) => write!(f, "Audio({})", bytes.len()),
            Self::FilesUnread => f.write_str("FilesUnread"),
            Self::Miss => f.write_str("Miss"),
        }
    }
}

/// Low 31 bits of `(channel, ts)`, and at least 1.
///
/// The voice folder is cleared when the inbox starts, so a hasher change
/// across a Rust upgrade does not leave a file the next play cannot find.
pub(super) fn handle(channel: &str, ts: &str) -> i32 {
    let mixed = BuildHasherDefault::<DefaultHasher>::default().hash_one(&(channel, ts));
    let low = u32::try_from(mixed & 0x7fff_ffff).unwrap_or(1);
    i32::try_from(low).unwrap_or(1).max(1)
}

/// The first playable file is the only one. A clip exists only when that
/// file already holds a `files.slack.com` address.
pub(super) fn present(text: &str, channel: &str, ts: &str, files: &[IncomingFile]) -> Shown {
    let candidate = files.iter().find(|file| is_candidate(file));
    let clip = candidate.and_then(|file| {
        file.url.clone().map(|url| Clip {
            handle: handle(channel, ts),
            url,
        })
    });
    let unlabeled = text.trim().is_empty() || text == "(no text)";
    let body = if !unlabeled {
        text.to_string()
    } else if let Some(file) = candidate {
        if is_voice(file) {
            "[voice message]".to_string()
        } else {
            "[audio]".to_string()
        }
    } else if files.is_empty() {
        "(no text)".to_string()
    } else {
        "[file]".to_string()
    };
    Shown { body, clip }
}

/// `FilesUnread` for a 403 or a short `missing_scope` body. `Audio` for a
/// short 200 body. Everything else is a miss.
///
/// A body past 4096 bytes is not a scope error just because it contains the
/// word. An audio file can.
pub(super) fn classify(status: u16, body: &[u8]) -> FetchClass {
    if files_unread(status, body) {
        FetchClass::FilesUnread
    } else if status == 200 && !body.is_empty() && body.len() <= VOICE_MAX_BYTES {
        FetchClass::Audio(body.to_vec())
    } else {
        FetchClass::Miss
    }
}

const SCOPE_BODY_MAX: usize = 4096;

fn files_unread(status: u16, body: &[u8]) -> bool {
    status == 403 || missing_scope(body)
}

fn missing_scope(body: &[u8]) -> bool {
    if body.len() > SCOPE_BODY_MAX || body.first() != Some(&b'{') {
        return false;
    }
    let mut index = 0;
    while index + 7 <= body.len() {
        if body[index..].starts_with(b"\"error\"") {
            let mut cursor = index + 7;
            while body.get(cursor) == Some(&b' ') {
                cursor += 1;
            }
            if body.get(cursor) == Some(&b':') {
                cursor += 1;
                while body.get(cursor) == Some(&b' ') {
                    cursor += 1;
                }
                if body[cursor..].starts_with(b"\"missing_scope\"") {
                    return true;
                }
            }
        }
        index += 1;
    }
    false
}

fn media_type(mimetype: &str) -> String {
    mimetype
        .split(';')
        .next()
        .unwrap_or(mimetype)
        .trim()
        .to_ascii_lowercase()
}

fn is_candidate(file: &IncomingFile) -> bool {
    let mime = media_type(&file.mimetype);
    if mime.starts_with("audio/") {
        return true;
    }
    audio_message_stem(&file.name) && (mime == "video/mp4" || mime == "video/webm")
}

fn is_voice(file: &IncomingFile) -> bool {
    let mime = media_type(&file.mimetype);
    audio_message_stem(&file.name)
        || mime == "audio/webm"
        || (mime.starts_with("audio/") && file.filetype.eq_ignore_ascii_case("webm"))
}

fn audio_message_stem(name: &str) -> bool {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let stem = base.rsplit_once('.').map_or(base, |(stem, _)| stem);
    stem == "audio_message"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(mimetype: &str, filetype: &str, name: &str, url: Option<&str>) -> IncomingFile {
        IncomingFile {
            mimetype: mimetype.into(),
            filetype: filetype.into(),
            name: name.into(),
            url: url.and_then(FilesHostUrl::parse),
        }
    }

    const HOST: &str = "https://files.slack.com/files-pri/T1/F1/download/clip";

    #[test]
    fn the_same_pair_repeats_and_a_different_pair_does_not() {
        let first = handle("C1", "1700000000.000100");
        assert!(first > 0);
        assert_eq!(first, handle("C1", "1700000000.000100"));
        assert_ne!(first, handle("C1", "1700000000.000101"));
        assert_ne!(handle("ab", "c"), handle("a", "bc"));
    }

    #[test]
    fn only_files_slack_com_is_a_host_address() {
        assert!(FilesHostUrl::parse(HOST).is_some());
        assert!(FilesHostUrl::parse("https://files.slack.com:443/a").is_some());
        assert!(FilesHostUrl::parse("https://FILES.SLACK.COM/a").is_some());
        assert!(FilesHostUrl::parse("https://example.com/a").is_none());
        assert!(FilesHostUrl::parse("https://files.slack.com.example.com/a").is_none());
        assert!(FilesHostUrl::parse("https://user:secret@files.slack.com/a").is_none());
        assert!(FilesHostUrl::parse("https://files.slack.com/a b").is_none());
        assert!(FilesHostUrl::parse("https://files.slack.com/a\\b").is_none());
        assert!(FilesHostUrl::parse("https://files.%73lack.com/a").is_none());
        assert!(FilesHostUrl::parse("http://files.slack.com/a").is_none());
        let url = FilesHostUrl::parse(HOST).expect("host");
        assert_eq!(format!("{url:?}"), "FilesHostUrl");
        assert_eq!(url.to_string(), "FilesHostUrl");
        assert!(!format!("{url:?}").contains("files.slack.com"));
        let row = file("audio/mpeg", "mp3", "song.mp3", Some(HOST));
        let shown = format!("{row:?}");
        assert!(shown.contains("audio/mpeg"));
        assert!(shown.contains("song.mp3"));
        assert!(!shown.contains("files.slack.com"));
    }

    #[test]
    fn an_audio_file_is_a_clip_and_a_voice_clip_is_named() {
        let mpeg = present(
            "",
            "C1",
            "1700000000.000100",
            &[file("audio/mpeg", "mp3", "song.mp3", Some(HOST))],
        );
        assert_eq!(mpeg.body, "[audio]");
        let clip = mpeg.clip.expect("clip");
        assert_eq!(clip.handle, handle("C1", "1700000000.000100"));
        assert!(clip.handle > 0);
        assert_eq!(clip.url.as_str(), HOST);

        let webm = present(
            "(no text)",
            "C1",
            "1.2",
            &[file("audio/webm", "webm", "audio_message.webm", Some(HOST))],
        );
        assert_eq!(webm.body, "[voice message]");
        assert!(webm.clip.is_some());

        let mp4 = present(
            "",
            "C1",
            "1.2",
            &[file("video/mp4", "mp4", "audio_message.mp4", Some(HOST))],
        );
        assert_eq!(mp4.body, "[voice message]");
        assert!(mp4.clip.is_some());

        let vacation = present(
            "",
            "C1",
            "1.2",
            &[file("video/mp4", "mp4", "vacation.mp4", Some(HOST))],
        );
        assert_eq!(vacation.body, "[file]");
        assert!(vacation.clip.is_none());

        let caption = present(
            "listen",
            "C1",
            "1.2",
            &[file("audio/mpeg", "mp3", "song.mp3", Some(HOST))],
        );
        assert_eq!(caption.body, "listen");
        assert!(caption.clip.is_some());

        let pdf = present(
            "",
            "C1",
            "1.2",
            &[file("application/pdf", "pdf", "notes.pdf", Some(HOST))],
        );
        assert_eq!(pdf.body, "[file]");
        assert!(pdf.clip.is_none());

        assert_eq!(present("   ", "C1", "1.2", &[]).body, "(no text)");
        assert!(present("", "C1", "1.2", &[]).clip.is_none());
    }

    #[test]
    fn the_first_playable_file_is_the_only_player() {
        let shown = present(
            "",
            "C1",
            "1.2",
            &[
                file("application/pdf", "pdf", "notes.pdf", None),
                file("audio/ogg", "ogg", "first.ogg", Some(HOST)),
                file(
                    "audio/mpeg",
                    "mp3",
                    "second.mp3",
                    Some("https://files.slack.com/other"),
                ),
            ],
        );
        assert_eq!(shown.body, "[audio]");
        assert_eq!(
            shown.clip.expect("first").url.as_str(),
            HOST,
            "a later file is not the player"
        );
        let unlabeled = present(
            "",
            "C1",
            "1.2",
            &[
                file(
                    "audio/mpeg",
                    "mp3",
                    "song.mp3",
                    Some("https://example.com/song.mp3"),
                ),
                file("audio/ogg", "ogg", "later.ogg", Some(HOST)),
            ],
        );
        assert_eq!(unlabeled.body, "[audio]");
        assert!(unlabeled.clip.is_none());
    }

    #[test]
    fn a_scope_error_is_not_audio_and_a_redirect_is_a_miss() {
        assert!(matches!(classify(403, b"nope"), FetchClass::FilesUnread));
        assert!(matches!(
            classify(200, br#"{"ok":false,"error":"missing_scope"}"#),
            FetchClass::FilesUnread
        ));
        assert!(matches!(
            classify(200, br#"{ "error" : "missing_scope" }"#),
            FetchClass::FilesUnread
        ));
        assert!(matches!(classify(404, b"{}"), FetchClass::Miss));
        assert!(matches!(classify(302, b""), FetchClass::Miss));
        assert!(matches!(classify(200, b""), FetchClass::Miss));
        assert_eq!(
            classify(200, b"ID3..."),
            FetchClass::Audio(b"ID3...".to_vec())
        );
        let mut long = vec![b'x'; SCOPE_BODY_MAX + 1];
        long.extend_from_slice(br#"{"error":"missing_scope"}"#);
        assert!(
            !matches!(classify(200, &long), FetchClass::FilesUnread),
            "a long body is not a scope error because it contains the word"
        );
        assert!(matches!(classify(500, &long), FetchClass::Miss));
        let huge = vec![0u8; VOICE_MAX_BYTES + 1];
        assert!(matches!(classify(200, &huge), FetchClass::Miss));
    }
}
