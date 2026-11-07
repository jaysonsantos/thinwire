use std::path::PathBuf;
use std::str::FromStr;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use quick_xml::Reader;
use quick_xml::events::Event as XmlEvent;

use super::{Action, BAD_SIGNATURE, CHECK_FAILED, PlainError};

/// The updater. One value, one writer.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum Update {
    /// This process will not check. `cargo run`, Linux, Windows, ad-hoc, bad `BUILD_ID`.
    Dormant,
    Checking {
        our: BuildId,
    },
    Quiet {
        our: BuildId,
        kind: QuietKind,
    },
    Offer {
        our: BuildId,
        offer: Offer,
    },
    Installing {
        our: BuildId,
        offer: Offer,
        phase: InstallPhase,
    },
    Failed {
        our: Option<BuildId>,
        error: PlainError,
    },
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum QuietKind {
    Current,
    /// The user chose Later for `skipped`. A feed event in this state is ignored.
    Later {
        skipped: BuildId,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum InstallPhase {
    Recheck,
    Fetching,
    Verifying,
    Armed,
}

#[derive(Debug)]
pub(super) enum Event {
    Action(Action),
    Feed(Result<Feed, PlainError>),
    Downloaded(Result<ArchiveFile, PlainError>),
    /// Probe and spawn finished. `Err` means the running bundle was not touched.
    Armed(Result<(), PlainError>),
}

#[derive(Debug)]
pub(super) enum Effect {
    None,
    FetchFeed {
        our: BuildId,
        purpose: FeedPurpose,
    },
    Download {
        offer: Offer,
    },
    /// `codesign` the extracted bundle, probe the parent directory, spawn the child.
    /// One effect so a second event cannot spawn a second child.
    VerifyAndArm {
        offer: Offer,
        archive: ArchiveFile,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum FeedPurpose {
    Check,
    BeforeInstall,
}

/// Workspace semver, commit count, and the 7 character sha, in that order.
/// `newer` reads only `commit_count`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct BuildId {
    semver: Semver,
    commit_count: u32,
    sha: Sha7,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Semver {
    major: u16,
    minor: u16,
    patch: u16,
}

/// Seven lowercase hex characters. Not a path and not a semver.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Sha7([u8; 7]);

impl BuildId {
    /// Accept `0.1.0.r812.gabc1234` with an optional trailing newline.
    /// Reject a nightly string in the semver slot, an uppercase sha, a leading
    /// zero on the count, and any extra field.
    pub(super) fn parse(line: &str) -> Result<Self, ParseError> {
        let line = strip_one_newline(line)?;
        let mut parts = line.split('.');
        let major = parse_uint(parts.next().ok_or(ParseError)?)?;
        let minor = parse_uint(parts.next().ok_or(ParseError)?)?;
        let patch = parse_uint(parts.next().ok_or(ParseError)?)?;
        let count = parse_count(parts.next().ok_or(ParseError)?)?;
        let sha = parse_sha(parts.next().ok_or(ParseError)?)?;
        if parts.next().is_some() {
            return Err(ParseError);
        }
        Ok(Self {
            semver: Semver {
                major,
                minor,
                patch,
            },
            commit_count: count,
            sha,
        })
    }

    #[must_use]
    pub(super) fn display(&self) -> String {
        let sha = std::str::from_utf8(&self.sha.0).expect("sha is ascii hex");
        format!(
            "{}.{}.{}.r{}.g{sha}",
            self.semver.major, self.semver.minor, self.semver.patch, self.commit_count
        )
    }
}

#[derive(Debug)]
pub(super) struct ParseError;

fn strip_one_newline(line: &str) -> Result<&str, ParseError> {
    let line = if let Some(rest) = line.strip_suffix('\n') {
        rest.strip_suffix('\r').unwrap_or(rest)
    } else {
        line
    };
    if line.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return Err(ParseError);
    }
    Ok(line)
}

fn parse_count(text: &str) -> Result<u32, ParseError> {
    let text = text.strip_prefix('r').ok_or(ParseError)?;
    parse_uint(text)
}

fn parse_sha(text: &str) -> Result<Sha7, ParseError> {
    let hex = text.strip_prefix('g').ok_or(ParseError)?;
    if hex.len() != 7
        || !hex
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(ParseError);
    }
    let mut bytes = [0; 7];
    bytes.copy_from_slice(hex.as_bytes());
    Ok(Sha7(bytes))
}

fn parse_semver(text: &str) -> Result<Semver, ParseError> {
    let mut parts = text.split('.');
    let major = parse_uint(parts.next().ok_or(ParseError)?)?;
    let minor = parse_uint(parts.next().ok_or(ParseError)?)?;
    let patch = parse_uint(parts.next().ok_or(ParseError)?)?;
    if parts.next().is_some() {
        return Err(ParseError);
    }
    Ok(Semver {
        major,
        minor,
        patch,
    })
}

fn parse_uint<T>(text: &str) -> Result<T, ParseError>
where
    T: FromStr,
{
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ParseError);
    }
    if text.len() > 1 && text.starts_with('0') {
        return Err(ParseError);
    }
    text.parse().map_err(|_| ParseError)
}

/// True only when the feed commit count is greater. Semver and sha do not order builds.
#[must_use]
pub(super) fn newer(feed: &BuildId, ours: &BuildId) -> bool {
    feed.commit_count > ours.commit_count
}

/// Plain notes, at most 280 characters, no control characters other than newline.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct Notes(String);

const NOT_A_RELEASE: &str = "This is a build from main, not a release.";
const NOTES_MAX: usize = 280;

impl Notes {
    pub(super) fn from_feed(raw: &str) -> Self {
        let mut text = String::new();
        for ch in raw.chars() {
            if ch != '\n' && ch.is_control() {
                continue;
            }
            if text.chars().count() == NOTES_MAX {
                break;
            }
            text.push(ch);
        }
        if text.is_empty() {
            text.push_str(NOT_A_RELEASE);
        }
        Self(text)
    }

    #[must_use]
    pub(super) fn text(&self) -> &str {
        &self.0
    }
}

/// One verified appcast item. The parser drops the XML after this exists.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct Offer {
    build: BuildId,
    notes: Notes,
    enclosure: Enclosure,
}

impl Offer {
    pub(super) fn view(&self) -> super::OfferView {
        super::OfferView {
            version: self.build.display(),
            notes: self.notes.text().to_string(),
        }
    }
}

/// HTTPS nightly URL for this exact build id. Other hosts never construct it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct Enclosure {
    url: NightlyUrl,
    length: u64,
    signature: [u8; 64],
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct NightlyUrl(String);

/// 512 MiB. The enclosure `length` and the download both stop here.
pub(super) const MAX_DOWNLOAD: u64 = 512 * 1024 * 1024;

/// The appcast after the detached signature has already been checked.
#[derive(Clone, Debug)]
pub(super) struct Feed {
    best: Option<Offer>,
}

impl Feed {
    fn decide(&self, our: &BuildId) -> Decision {
        match &self.best {
            Some(offer) if newer(&offer.build, our) => Decision::Available(offer.clone()),
            _ => Decision::Current,
        }
    }
}

pub(super) enum Decision {
    Current,
    Available(Offer),
}

/// Bytes on disk inside the staging directory. Not yet a bundle.
#[derive(Debug)]
#[allow(dead_code)]
pub(super) struct ArchiveFile {
    path: PathBuf,
}

const MAX_APPCAST: usize = 1024 * 1024;

/// Verify `signature` over the exact appcast bytes, then parse.
/// A bad signature is [`BAD_SIGNATURE`] and does not fall through to an older item.
pub(super) fn open_feed(
    appcast: &[u8],
    signature: &[u8; 64],
    key: &VerifyingKey,
) -> Result<Feed, PlainError> {
    if appcast.len() > MAX_APPCAST {
        return Err(CHECK_FAILED);
    }
    verify_detached(appcast, signature, key)?;
    parse_appcast(appcast)
}

pub(super) fn verify_detached(
    message: &[u8],
    signature: &[u8; 64],
    key: &VerifyingKey,
) -> Result<(), PlainError> {
    let signature = Signature::from_bytes(signature);
    key.verify(message, &signature).map_err(|_| BAD_SIGNATURE)
}

fn nightly_url(build: &BuildId) -> String {
    format!(
        "https://github.com/jaysonsantos/thinwire/releases/download/nightly/thinwire-macos-arm64-{}.tar.gz",
        build.display()
    )
}

fn parse_appcast(appcast: &[u8]) -> Result<Feed, PlainError> {
    if declares_dtd(appcast) {
        return Err(CHECK_FAILED);
    }
    let text = std::str::from_utf8(appcast).map_err(|_| CHECK_FAILED)?;
    let mut reader = Reader::from_str(text);
    let config = reader.config_mut();
    config.trim_text_start = false;
    config.trim_text_end = false;
    config.expand_empty_elements = false;
    let mut buf = Vec::new();
    let mut best = None;
    let mut draft: Option<Draft> = None;
    let mut capture = Capture::None;
    loop {
        let event = reader.read_event_into(&mut buf).map_err(|_| CHECK_FAILED)?;
        match event {
            XmlEvent::Start(element) => {
                let name = local_name(element.name().as_ref());
                if name == b"item" {
                    draft = Some(Draft::default());
                    capture = Capture::None;
                } else if name == b"enclosure" {
                    if let Some(draft) = draft.as_mut() {
                        read_enclosure(&element, draft)?;
                    }
                } else if draft.is_some() {
                    capture = capture_of(&name);
                }
            }
            XmlEvent::Empty(element) => {
                let name = local_name(element.name().as_ref());
                if name == b"enclosure" {
                    if let Some(draft) = draft.as_mut() {
                        read_enclosure(&element, draft)?;
                    }
                }
            }
            XmlEvent::Text(text) => {
                if let Some(draft) = draft.as_mut() {
                    let text = text.xml_content().map_err(|_| CHECK_FAILED)?;
                    append(draft, capture, &text);
                }
            }
            XmlEvent::CData(text) => {
                if let Some(draft) = draft.as_mut() {
                    let text = std::str::from_utf8(text.as_ref()).map_err(|_| CHECK_FAILED)?;
                    append(draft, capture, text);
                }
            }
            XmlEvent::End(element) => {
                let name = local_name(element.name().as_ref());
                if name == b"item" {
                    if let Some(draft) = draft.take() {
                        if let Some(offer) = offer_from(&draft) {
                            consider(&mut best, offer);
                        }
                    }
                    capture = Capture::None;
                } else if capture_of(&name) == capture {
                    capture = Capture::None;
                }
            }
            XmlEvent::Eof => break,
            XmlEvent::DocType(_) => return Err(CHECK_FAILED),
            _ => {}
        }
        buf.clear();
    }
    Ok(Feed { best })
}

fn declares_dtd(bytes: &[u8]) -> bool {
    let lower = bytes.to_ascii_lowercase();
    contains_token(&lower, b"<!doctype") || contains_token(&lower, b"<!entity")
}

fn contains_token(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn local_name(name: &[u8]) -> Vec<u8> {
    match name.iter().rposition(|byte| *byte == b':') {
        Some(index) => name[index + 1..].to_vec(),
        None => name.to_vec(),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Capture {
    None,
    Version,
    ShortVersion,
    Build,
    Description,
}

fn capture_of(name: &[u8]) -> Capture {
    match name {
        b"version" => Capture::Version,
        b"shortVersionString" => Capture::ShortVersion,
        b"build" => Capture::Build,
        b"description" => Capture::Description,
        _ => Capture::None,
    }
}

#[derive(Default)]
struct Draft {
    version: String,
    short_version: String,
    build: String,
    description: String,
    url: String,
    signature: String,
    length: String,
    os: String,
    kind: String,
}

fn append(draft: &mut Draft, capture: Capture, text: &str) {
    let dest = match capture {
        Capture::None => return,
        Capture::Version => &mut draft.version,
        Capture::ShortVersion => &mut draft.short_version,
        Capture::Build => &mut draft.build,
        Capture::Description => &mut draft.description,
    };
    dest.push_str(text);
}

fn read_enclosure(
    element: &quick_xml::events::BytesStart<'_>,
    draft: &mut Draft,
) -> Result<(), PlainError> {
    for attr in element.attributes() {
        let attr = attr.map_err(|_| CHECK_FAILED)?;
        let key = local_name(attr.key.as_ref());
        let value = attr.unescape_value().map_err(|_| CHECK_FAILED)?;
        match key.as_slice() {
            b"url" => draft.url = value.into_owned(),
            b"edSignature" => draft.signature = value.into_owned(),
            b"length" => draft.length = value.into_owned(),
            b"os" => draft.os = value.into_owned(),
            b"type" => draft.kind = value.into_owned(),
            _ => {}
        }
    }
    Ok(())
}

fn offer_from(draft: &Draft) -> Option<Offer> {
    let build = BuildId::parse(draft.build.trim()).ok()?;
    let version = parse_semver(draft.version.trim()).ok()?;
    let short = parse_semver(draft.short_version.trim()).ok()?;
    if version != build.semver || short != version {
        return None;
    }
    if draft.os.trim() != "macos" || draft.kind.trim() != "application/gzip" {
        return None;
    }
    let length: u64 = draft.length.trim().parse().ok()?;
    if length == 0 || length > MAX_DOWNLOAD {
        return None;
    }
    let url = draft.url.trim();
    if url != nightly_url(&build) {
        return None;
    }
    let signature = decode_signature(draft.signature.trim())?;
    Some(Offer {
        build,
        notes: Notes::from_feed(&draft.description),
        enclosure: Enclosure {
            url: NightlyUrl(url.to_string()),
            length,
            signature,
        },
    })
}

fn decode_signature(text: &str) -> Option<[u8; 64]> {
    let bytes = STANDARD.decode(text).ok()?;
    bytes.try_into().ok()
}

fn consider(best: &mut Option<Offer>, offer: Offer) {
    let replace = match best {
        Some(current) => offer.build.commit_count > current.build.commit_count,
        None => true,
    };
    if replace {
        *best = Some(offer);
    }
}

/// `step` ignores an action or a feed result the current state did not ask for.
/// A second Install while `Installing` returns the same state and [`Effect::None`].
pub(super) fn step(state: Update, event: Event) -> (Update, Effect) {
    match (state, event) {
        (Update::Checking { our }, Event::Feed(Ok(feed))) => match feed.decide(&our) {
            Decision::Available(offer) => (Update::Offer { our, offer }, Effect::None),
            Decision::Current => (
                Update::Quiet {
                    our,
                    kind: QuietKind::Current,
                },
                Effect::None,
            ),
        },
        (Update::Checking { our }, Event::Feed(Err(error))) => (
            Update::Failed {
                our: Some(our),
                error,
            },
            Effect::None,
        ),
        (Update::Offer { our, offer }, Event::Action(Action::Later)) => (
            Update::Quiet {
                our,
                kind: QuietKind::Later {
                    skipped: offer.build,
                },
            },
            Effect::None,
        ),
        (Update::Offer { our, offer }, Event::Action(Action::Install)) => (
            Update::Installing {
                our: our.clone(),
                offer,
                phase: InstallPhase::Recheck,
            },
            Effect::FetchFeed {
                our,
                purpose: FeedPurpose::BeforeInstall,
            },
        ),
        (
            Update::Installing {
                our,
                offer,
                phase: InstallPhase::Recheck,
            },
            Event::Feed(Ok(feed)),
        ) => match feed.decide(&our) {
            Decision::Current => (
                Update::Quiet {
                    our,
                    kind: QuietKind::Current,
                },
                Effect::None,
            ),
            Decision::Available(fresh) if fresh.build == offer.build => (
                Update::Installing {
                    our,
                    offer,
                    phase: InstallPhase::Fetching,
                },
                Effect::Download { offer: fresh },
            ),
            Decision::Available(fresh) => (Update::Offer { our, offer: fresh }, Effect::None),
        },
        (
            Update::Installing {
                phase: InstallPhase::Recheck,
                our,
                ..
            },
            Event::Feed(Err(error)),
        ) => (
            Update::Failed {
                our: Some(our),
                error,
            },
            Effect::None,
        ),
        (
            Update::Installing {
                our,
                offer,
                phase: InstallPhase::Fetching,
            },
            Event::Downloaded(Ok(archive)),
        ) => (
            Update::Installing {
                our,
                offer: offer.clone(),
                phase: InstallPhase::Verifying,
            },
            Effect::VerifyAndArm { offer, archive },
        ),
        (
            Update::Installing {
                phase: InstallPhase::Verifying,
                our,
                offer,
            },
            Event::Armed(Ok(())),
        ) => (
            Update::Installing {
                our,
                offer,
                phase: InstallPhase::Armed,
            },
            Effect::None,
        ),
        (
            Update::Installing { our, phase, .. },
            Event::Downloaded(Err(error)) | Event::Armed(Err(error)),
        ) if phase != InstallPhase::Armed => (
            Update::Failed {
                our: Some(our),
                error,
            },
            Effect::None,
        ),
        (
            Update::Failed {
                our: Some(our),
                error,
            },
            Event::Action(Action::Retry),
        ) if error.retry => (
            Update::Checking { our: our.clone() },
            Effect::FetchFeed {
                our,
                purpose: FeedPurpose::Check,
            },
        ),
        (Update::Failed { our: Some(our), .. }, Event::Action(Action::Dismiss)) => (
            Update::Quiet {
                our,
                kind: QuietKind::Current,
            },
            Effect::None,
        ),
        (Update::Failed { our: None, .. }, Event::Action(Action::Dismiss)) => {
            (Update::Dormant, Effect::None)
        }
        (state, _) => (state, Effect::None),
    }
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::{Signer, SigningKey};

    use super::super::{BAD_SIGNATURE, CHECK_FAILED, project};
    use super::*;

    fn id(line: &str) -> BuildId {
        BuildId::parse(line).expect("build id")
    }

    fn key() -> SigningKey {
        SigningKey::from_bytes(&[4; 32])
    }

    fn signature_text(bytes: &[u8; 64]) -> String {
        STANDARD.encode(bytes)
    }

    fn item(build: &BuildId, notes: &str, url: &str, signature: &str, length: u64) -> String {
        let version = format!(
            "{}.{}.{}",
            build.semver.major, build.semver.minor, build.semver.patch
        );
        let shown = build.display();
        format!(
            "<item>\
<title>{shown}</title>\
<sparkle:version>{version}</sparkle:version>\
<sparkle:shortVersionString>{version}</sparkle:shortVersionString>\
<thinwire:build>{shown}</thinwire:build>\
<description>{notes}</description>\
<enclosure url=\"{url}\" sparkle:edSignature=\"{signature}\" sparkle:os=\"macos\" length=\"{length}\" type=\"application/gzip\"/>\
</item>"
        )
    }

    fn appcast(items: &str) -> String {
        format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
<rss version=\"2.0\" xmlns:sparkle=\"http://www.andymatuschak.org/xml-namespaces/sparkle\" xmlns:thinwire=\"https://github.com/jaysonsantos/thinwire\">\
<channel>{items}</channel></rss>"
        )
    }

    fn open_signed(xml: &str) -> Result<Feed, PlainError> {
        let signing = key();
        let signature = signing.sign(xml.as_bytes()).to_bytes();
        open_feed(xml.as_bytes(), &signature, &signing.verifying_key())
    }

    fn sample_offer(count: u32, sha: &str) -> Offer {
        let build = id(&format!("0.1.0.r{count}.g{sha}"));
        Offer {
            build: build.clone(),
            notes: Notes::from_feed("notes"),
            enclosure: Enclosure {
                url: NightlyUrl(nightly_url(&build)),
                length: 10,
                signature: [1; 64],
            },
        }
    }

    #[test]
    fn newer_is_the_commit_count() {
        let feed = id("0.1.0.r10.gabc1234");
        let ours = id("0.2.0.r9.gabc1234");
        assert!(newer(&feed, &ours));
        assert!(!newer(&ours, &feed));
        assert_eq!(feed.display(), "0.1.0.r10.gabc1234");
    }

    #[test]
    fn an_equal_count_is_not_newer() {
        let feed = id("0.1.0.r10.gabc1234");
        let ours = id("0.9.0.r10.gdeadbee");
        assert!(!newer(&feed, &ours));
        assert!(!newer(&ours, &feed));
    }

    #[test]
    fn build_id_rejects_a_nightly_string_in_the_semver_slot() {
        assert!(BuildId::parse("0.1.0.r812.gabc1234").is_ok());
        assert!(BuildId::parse("0.1.0.r812.gabc1234\n").is_ok());
        assert!(BuildId::parse("0.1.0.r812.gabc1234\r\n").is_ok());
        assert!(BuildId::parse("0.1.0.r812.gabc1234.r9.gdeadbee").is_err());
        assert!(BuildId::parse("0.1.0.r812.gABC1234").is_err());
        assert!(BuildId::parse("0.1.0.r0812.gabc1234").is_err());
        assert!(BuildId::parse("0.1.0.r812.gabc1234.extra").is_err());
        assert!(BuildId::parse("01.0.0.r1.gabc1234").is_err());
    }

    #[test]
    fn notes_drop_controls_and_use_the_not_a_release_sentence() {
        assert_eq!(
            Notes::from_feed("").text(),
            "This is a build from main, not a release."
        );
        assert_eq!(Notes::from_feed("a\u{0001}b\nc").text(), "ab\nc");
        assert_eq!(
            Notes::from_feed(&"x".repeat(300)).text().chars().count(),
            280
        );
    }

    #[test]
    fn a_bad_appcast_signature_is_not_an_offer() {
        let low = id("0.1.0.r2.g1111111");
        let high = id("0.1.0.r3.g2222222");
        let archive_sig = signature_text(&[7; 64]);
        let xml = appcast(&format!(
            "{}{}",
            item(&low, "older", &nightly_url(&low), &archive_sig, 4),
            item(&high, "newer", &nightly_url(&high), &archive_sig, 4),
        ));
        let signing = key();
        let mut signature = signing.sign(xml.as_bytes()).to_bytes();
        signature[0] ^= 0xff;
        let error = open_feed(xml.as_bytes(), &signature, &signing.verifying_key()).unwrap_err();
        assert_eq!(error, BAD_SIGNATURE);
    }

    #[test]
    fn an_enclosure_on_another_host_is_rejected() {
        let build = id("0.1.0.r4.gabc1234");
        let archive_sig = signature_text(&[7; 64]);
        let wrong = item(
            &build,
            "notes",
            "https://example.com/thinwire-macos-arm64-0.1.0.r4.gabc1234.tar.gz",
            &archive_sig,
            12,
        );
        let feed = open_signed(&appcast(&wrong)).expect("signed appcast");
        assert!(feed.best.is_none());

        let query = format!("{}?token=1", nightly_url(&build));
        let queried = item(&build, "notes", &query, &archive_sig, 12);
        let feed = open_signed(&appcast(&queried)).expect("signed appcast");
        assert!(feed.best.is_none());
    }

    #[test]
    fn the_feed_keeps_the_greatest_build_only() {
        let low_semver_high_count = id("0.1.0.r20.g4444444");
        let high_semver_low_count = id("0.9.0.r5.g2222222");
        let nightly_in_version = id("0.1.0.r30.g3333333");
        let other_host = id("0.1.0.r40.g5555555");
        let too_big = id("0.1.0.r50.g6666666");
        let winner_sig = [9; 64];
        let other_sig = signature_text(&[3; 64]);
        let mut nightly_item = item(
            &nightly_in_version,
            "skip",
            &nightly_url(&nightly_in_version),
            &other_sig,
            8,
        );
        let nightly_version = nightly_in_version.display();
        nightly_item = nightly_item.replace(
            "<sparkle:version>0.1.0</sparkle:version>",
            &format!("<sparkle:version>{nightly_version}</sparkle:version>"),
        );
        let notes = "Fix the list.\u{0007}\nThis is a build from main, not a release.";
        let xml = appcast(&format!(
            "{}{}{}{}{}",
            item(
                &high_semver_low_count,
                "old",
                &nightly_url(&high_semver_low_count),
                &other_sig,
                8,
            ),
            nightly_item,
            item(
                &other_host,
                "elsewhere",
                "https://objects.githubusercontent.com/thinwire.tar.gz",
                &other_sig,
                8,
            ),
            item(
                &too_big,
                "huge",
                &nightly_url(&too_big),
                &other_sig,
                MAX_DOWNLOAD + 1,
            ),
            item(
                &low_semver_high_count,
                notes,
                &nightly_url(&low_semver_high_count),
                &signature_text(&winner_sig),
                44,
            ),
        ));
        let feed = open_signed(&xml).expect("signed appcast");
        let offer = feed.best.clone().expect("one offer");
        assert_eq!(offer.build.display(), "0.1.0.r20.g4444444");
        assert_eq!(
            offer.notes.text(),
            "Fix the list.\nThis is a build from main, not a release."
        );
        assert_eq!(offer.enclosure.length, 44);
        assert_eq!(offer.enclosure.signature, winner_sig);
        assert_eq!(offer.enclosure.url.0, nightly_url(&low_semver_high_count));

        let same = id("0.1.0.r20.g4444444");
        assert!(matches!(feed.decide(&same), Decision::Current));
        let older = id("0.1.0.r19.g4444444");
        match feed.decide(&older) {
            Decision::Available(offer) => {
                assert_eq!(offer.build.display(), "0.1.0.r20.g4444444")
            }
            Decision::Current => panic!("a greater count is an offer"),
        }
    }

    #[test]
    fn a_second_install_does_not_arm_twice() {
        let state = Update::Installing {
            our: id("0.1.0.r1.g0000001"),
            offer: sample_offer(2, "0000002"),
            phase: InstallPhase::Fetching,
        };
        let (next, effect) = step(state.clone(), Event::Action(Action::Install));
        assert_eq!(next, state);
        assert!(matches!(effect, Effect::None));
    }

    #[test]
    fn a_recheck_with_a_higher_build_returns_to_the_offer() {
        let our = id("0.1.0.r1.g0000001");
        let state = Update::Installing {
            our: our.clone(),
            offer: sample_offer(2, "0000002"),
            phase: InstallPhase::Recheck,
        };
        let fresh = sample_offer(3, "0000003");
        let (next, effect) = step(
            state,
            Event::Feed(Ok(Feed {
                best: Some(fresh.clone()),
            })),
        );
        assert!(matches!(effect, Effect::None));
        match next {
            Update::Offer { our: got, offer } => {
                assert_eq!(got, our);
                assert_eq!(offer.build, fresh.build);
            }
            _ => panic!("a higher recheck is an offer again"),
        }
    }

    #[test]
    fn later_ignores_a_feed_event() {
        let state = Update::Quiet {
            our: id("0.1.0.r1.g0000001"),
            kind: QuietKind::Later {
                skipped: id("0.1.0.r4.g0000004"),
            },
        };
        let (next, effect) = step(
            state.clone(),
            Event::Feed(Ok(Feed {
                best: Some(sample_offer(9, "0000009")),
            })),
        );
        assert_eq!(next, state);
        assert!(matches!(effect, Effect::None));
    }

    #[test]
    fn a_failed_check_does_not_record_a_bundle_change() {
        let our = id("0.1.0.r1.gabc1234");
        let (next, effect) = step(
            Update::Checking { our: our.clone() },
            Event::Feed(Err(CHECK_FAILED)),
        );
        assert!(matches!(effect, Effect::None));
        assert_eq!(
            next,
            Update::Failed {
                our: Some(our),
                error: CHECK_FAILED,
            }
        );
        assert_eq!(project(&next), super::super::Prompt::Failed(CHECK_FAILED));
    }
}
