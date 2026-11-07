use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use quick_xml::Reader;
use quick_xml::events::Event as XmlEvent;

use super::{Action, BAD_SIGNATURE, CHECK_FAILED, PlainError};

#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum Update {
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
    Later { skipped: BuildId },
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
    Armed(Result<(), PlainError>),
}

#[derive(Debug)]
pub(super) enum Effect {
    None,
    FetchFeed { our: BuildId, purpose: FeedPurpose },
    Download { offer: Offer },
    VerifyAndArm { offer: Offer, archive: ArchiveFile },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum FeedPurpose {
    Check,
    BeforeInstall,
}

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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Sha7([u8; 7]);

impl BuildId {
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

#[must_use]
pub(super) fn newer(feed: &BuildId, ours: &BuildId) -> bool {
    feed.commit_count > ours.commit_count
}

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

#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct Offer {
    build: BuildId,
    notes: Notes,
    enclosure: Enclosure,
}

impl Offer {
    #[cfg(target_os = "macos")]
    pub(super) fn build(&self) -> &BuildId {
        &self.build
    }

    #[cfg(target_os = "macos")]
    pub(super) fn enclosure(&self) -> &Enclosure {
        &self.enclosure
    }

    pub(super) fn view(&self) -> super::OfferView {
        super::OfferView {
            version: self.build.display(),
            notes: self.notes.text().to_string(),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct Enclosure {
    url: NightlyUrl,
    length: u64,
    signature: [u8; 64],
}

impl Enclosure {
    #[cfg(target_os = "macos")]
    pub(super) fn url(&self) -> &str {
        self.url.as_str()
    }

    #[cfg(target_os = "macos")]
    pub(super) fn length(&self) -> u64 {
        self.length
    }

    #[cfg(target_os = "macos")]
    pub(super) fn signature(&self) -> &[u8; 64] {
        &self.signature
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct NightlyUrl {
    url: String,
}

impl NightlyUrl {
    fn for_build(build: &BuildId) -> Self {
        Self {
            url: nightly_url(build),
        }
    }

    fn as_str(&self) -> &str {
        &self.url
    }
}

pub(super) const MAX_DOWNLOAD: u64 = 512 * 1024 * 1024;

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

#[derive(Debug)]
pub(super) struct ArchiveFile {
    path: PathBuf,
}

impl ArchiveFile {
    pub(super) fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }
}

pub(super) const MAX_APPCAST: usize = 1024 * 1024;
pub(super) const MAX_EXTRACT: u64 = 1024 * 1024 * 1024;

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
                if name == b"enclosure"
                    && let Some(draft) = draft.as_mut()
                {
                    read_enclosure(&element, draft)?;
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
                    if let Some(draft) = draft.take()
                        && let Some(offer) = offer_from(&draft)
                    {
                        consider(&mut best, offer);
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
    let url = NightlyUrl::for_build(&build);
    if draft.url.trim() != url.as_str() {
        return None;
    }
    let signature = decode_signature(draft.signature.trim())?;
    Some(Offer {
        build,
        notes: Notes::from_feed(&draft.description),
        enclosure: Enclosure {
            url,
            length,
            signature,
        },
    })
}

pub(super) fn decode_signature(text: &str) -> Option<[u8; 64]> {
    let bytes = STANDARD.decode(text.trim()).ok()?;
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

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Swapped {
    Installed,
    RolledBack,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum SwapError {
    NotWritable,
    Missing,
}

pub(super) fn converge(
    staged: &Path,
    fresh: &Path,
    dest: &Path,
    previous: &Path,
) -> Result<Swapped, SwapError> {
    let staged_here = real_dir(staged);
    let fresh_here = real_dir(fresh);
    let dest_here = real_dir(dest);
    let previous_here = real_dir(previous);

    if staged_here && dest_here {
        place(staged, fresh)?;
        return finish_replace(fresh, dest, previous);
    }
    if fresh_here && dest_here {
        return finish_replace(fresh, dest, previous);
    }
    if fresh_here && !dest_here {
        place(fresh, dest)?;
        return Ok(Swapped::Installed);
    }
    if !fresh_here && !staged_here && !dest_here && previous_here {
        place(previous, dest)?;
        return Ok(Swapped::RolledBack);
    }
    if !fresh_here && !staged_here && dest_here {
        if previous_here {
            remove_path(previous)?;
        }
        return Ok(Swapped::Installed);
    }
    if staged_here && !dest_here && !fresh_here {
        place(staged, dest)?;
        return Ok(Swapped::Installed);
    }
    Err(SwapError::Missing)
}

fn finish_replace(fresh: &Path, dest: &Path, previous: &Path) -> Result<Swapped, SwapError> {
    if real_dir(previous) {
        remove_path(previous)?;
    }
    place(dest, previous)?;
    if let Err(error) = place(fresh, dest) {
        let _ = place(previous, dest);
        return Err(error);
    }
    Ok(Swapped::Installed)
}

fn place(from: &Path, to: &Path) -> Result<(), SwapError> {
    let marker = ready_marker(to);
    if path_exists(to) {
        if marker.is_file() {
            if path_exists(from) {
                remove_path(from)?;
            }
            let _ = fs::remove_file(&marker);
            return Ok(());
        }
        if path_exists(from) {
            remove_path(to)?;
        } else {
            return Ok(());
        }
    }
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::CrossesDevices => {
            copy_tree(from, to).map_err(|_| SwapError::NotWritable)?;
            write_marker(&marker)?;
            remove_path(from)?;
            let _ = fs::remove_file(&marker);
            Ok(())
        }
        Err(_) => Err(SwapError::NotWritable),
    }
}

fn ready_marker(to: &Path) -> PathBuf {
    let mut name = to.file_name().unwrap_or_default().to_os_string();
    name.push(".ready");
    match to.parent() {
        Some(parent) => parent.join(name),
        None => PathBuf::from(name),
    }
}

fn real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|meta| meta.is_dir())
        .unwrap_or(false)
}

fn path_exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn remove_path(path: &Path) -> Result<(), SwapError> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(SwapError::NotWritable),
    };
    let result = if meta.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    result.map_err(|_| SwapError::NotWritable)
}

fn write_marker(path: &Path) -> Result<(), SwapError> {
    let mut file = File::create(path).map_err(|_| SwapError::NotWritable)?;
    file.write_all(b"ready")
        .map_err(|_| SwapError::NotWritable)?;
    file.sync_all().map_err(|_| SwapError::NotWritable)?;
    Ok(())
}

fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(from)?;
    if meta.file_type().is_symlink() {
        return copy_symlink(from, to);
    }
    if meta.is_dir() {
        fs::create_dir(to)?;
        fs::set_permissions(to, meta.permissions())?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            copy_tree(&entry.path(), &to.join(entry.file_name()))?;
        }
        return Ok(());
    }
    fs::copy(from, to)?;
    Ok(())
}

#[cfg(unix)]
fn copy_symlink(from: &Path, to: &Path) -> io::Result<()> {
    let target = fs::read_link(from)?;
    std::os::unix::fs::symlink(target, to)
}

#[cfg(not(unix))]
fn copy_symlink(_from: &Path, _to: &Path) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "symlink copy"))
}

const REDIRECT_HOSTS: &[&str] = &[
    "release-assets.githubusercontent.com",
    "objects.githubusercontent.com",
    "github-releases.githubusercontent.com",
];

pub(super) const MAX_REDIRECTS: u32 = 3;

pub(super) fn follow_redirect(
    current: &str,
    location: &str,
    hops_already: u32,
) -> Result<String, PlainError> {
    if hops_already >= MAX_REDIRECTS {
        return Err(CHECK_FAILED);
    }
    let base = url::Url::parse(current).map_err(|_| CHECK_FAILED)?;
    let next = base.join(location).map_err(|_| CHECK_FAILED)?;
    if next.scheme() != "https" || next.port().is_some() {
        return Err(CHECK_FAILED);
    }
    if !next.username().is_empty() || next.password().is_some() {
        return Err(CHECK_FAILED);
    }
    let host = next.host_str().unwrap_or("");
    if !REDIRECT_HOSTS.contains(&host) {
        return Err(CHECK_FAILED);
    }
    Ok(next.to_string())
}

pub(super) fn developer_id_text(text: &str) -> bool {
    let mut team = false;
    let mut identifier = false;
    let mut authority = false;
    for line in text.lines() {
        let line = line.trim();
        if line == "TeamIdentifier=89SP4VB8Y7" {
            team = true;
        }
        if line == "Identifier=dev.jaysonsantos.thinwire" {
            identifier = true;
        }
        if line.starts_with("Authority=Developer ID Application:") {
            authority = true;
        }
    }
    team && identifier && authority
}

pub(super) fn extract_archive(archive: &Path, dest_dir: &Path) -> Result<PathBuf, PlainError> {
    let bundle = dest_dir.join("Thinwire.app");
    if path_exists(&bundle) {
        remove_path(&bundle).map_err(|_| CHECK_FAILED)?;
    }
    fs::create_dir_all(dest_dir).map_err(|_| CHECK_FAILED)?;
    let extracted = extract_members(archive, dest_dir);
    if extracted.is_err() {
        let _ = remove_path(&bundle);
    }
    extracted?;
    if !real_dir(&bundle) {
        let _ = remove_path(&bundle);
        return Err(BAD_SIGNATURE);
    }
    Ok(bundle)
}

fn extract_members(archive: &Path, dest_dir: &Path) -> Result<(), PlainError> {
    let file = File::open(archive).map_err(|_| BAD_SIGNATURE)?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(decoder);
    let bundle = dest_dir.join("Thinwire.app");
    let mut written = 0u64;
    let entries = tar.entries().map_err(|_| BAD_SIGNATURE)?;
    for entry in entries {
        let mut entry = entry.map_err(|_| BAD_SIGNATURE)?;
        let path = entry.path().map_err(|_| BAD_SIGNATURE)?.into_owned();
        let rel = safe_member(&path)?;
        let out = dest_dir.join(&rel);
        if !out.starts_with(dest_dir) {
            return Err(BAD_SIGNATURE);
        }
        match entry.header().entry_type() {
            tar::EntryType::Directory => {
                fs::create_dir_all(&out).map_err(|_| CHECK_FAILED)?;
            }
            tar::EntryType::Regular => {
                if let Some(parent) = out.parent() {
                    fs::create_dir_all(parent).map_err(|_| CHECK_FAILED)?;
                }
                let declared = entry.size();
                if declared > MAX_EXTRACT || written.saturating_add(declared) > MAX_EXTRACT {
                    return Err(BAD_SIGNATURE);
                }
                let mut file = File::create(&out).map_err(|_| CHECK_FAILED)?;
                let copied = io::copy(&mut entry, &mut file).map_err(|_| BAD_SIGNATURE)?;
                written = written.saturating_add(copied);
                if written > MAX_EXTRACT || copied != declared {
                    return Err(BAD_SIGNATURE);
                }
                set_mode(&out, entry.header().mode().unwrap_or(0o644));
            }
            tar::EntryType::Symlink => {
                let target = entry
                    .link_name()
                    .map_err(|_| BAD_SIGNATURE)?
                    .ok_or(BAD_SIGNATURE)?;
                if !link_stays_inside(&bundle, &rel, &target) {
                    return Err(BAD_SIGNATURE);
                }
                if let Some(parent) = out.parent() {
                    fs::create_dir_all(parent).map_err(|_| CHECK_FAILED)?;
                }
                symlink_at(&target, &out)?;
            }
            _ => return Err(BAD_SIGNATURE),
        }
    }
    Ok(())
}

fn safe_member(name: &Path) -> Result<PathBuf, PlainError> {
    let mut out = PathBuf::new();
    for component in name.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            _ => return Err(BAD_SIGNATURE),
        }
    }
    match out.components().next() {
        Some(Component::Normal(part)) if part == "Thinwire.app" => Ok(out),
        _ => Err(BAD_SIGNATURE),
    }
}

fn link_stays_inside(bundle: &Path, link_rel: &Path, target: &Path) -> bool {
    if target.is_absolute() {
        return false;
    }
    let Some(bundle_parent) = bundle.parent() else {
        return false;
    };
    let Some(link_dir) = bundle_parent.join(link_rel).parent().map(Path::to_path_buf) else {
        return false;
    };
    let mut resolved = link_dir;
    for component in target.components() {
        match component {
            Component::Normal(part) => resolved.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                if !resolved.pop() {
                    return false;
                }
            }
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    resolved.starts_with(bundle)
}

#[cfg(unix)]
fn symlink_at(target: &Path, out: &Path) -> Result<(), PlainError> {
    std::os::unix::fs::symlink(target, out).map_err(|_| BAD_SIGNATURE)
}

#[cfg(not(unix))]
fn symlink_at(_target: &Path, _out: &Path) -> Result<(), PlainError> {
    Err(BAD_SIGNATURE)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o777));
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) {}

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
                url: NightlyUrl::for_build(&build),
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
        assert_eq!(
            offer.enclosure.url.as_str(),
            nightly_url(&low_semver_high_count)
        );

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
    fn install_fetches_the_feed_again_before_downloading() {
        let our = id("0.1.0.r1.g0000001");
        let (_, effect) = step(
            Update::Offer {
                our: our.clone(),
                offer: sample_offer(2, "0000002"),
            },
            Event::Action(Action::Install),
        );
        match effect {
            Effect::FetchFeed { our: got, purpose } => {
                assert_eq!(got, our);
                assert_eq!(purpose, FeedPurpose::BeforeInstall);
            }
            _ => panic!("install rechecks the feed"),
        }
    }

    #[test]
    fn a_bad_download_does_not_arm() {
        let (next, effect) = step(
            Update::Installing {
                our: id("0.1.0.r1.g0000001"),
                offer: sample_offer(2, "0000002"),
                phase: InstallPhase::Fetching,
            },
            Event::Downloaded(Err(CHECK_FAILED)),
        );
        assert!(matches!(effect, Effect::None));
        assert!(matches!(
            next,
            Update::Failed {
                error: CHECK_FAILED,
                ..
            }
        ));
    }

    #[test]
    fn a_downloaded_archive_is_what_gets_verified() {
        let archive = ArchiveFile::new(PathBuf::from("/tmp/thinwire-update.tar.gz"));
        let offer = sample_offer(2, "0000002");
        let (next, effect) = step(
            Update::Installing {
                our: id("0.1.0.r1.g0000001"),
                offer: offer.clone(),
                phase: InstallPhase::Fetching,
            },
            Event::Downloaded(Ok(archive)),
        );
        match effect {
            Effect::VerifyAndArm {
                offer: got,
                archive,
            } => {
                assert_eq!(got.build, offer.build);
                assert_eq!(archive.path(), Path::new("/tmp/thinwire-update.tar.gz"));
            }
            _ => panic!("a download continues to verify"),
        }
        assert!(matches!(
            next,
            Update::Installing {
                phase: InstallPhase::Verifying,
                ..
            }
        ));
    }

    #[test]
    fn an_armed_install_asks_the_window_to_quit() {
        let (next, effect) = step(
            Update::Installing {
                our: id("0.1.0.r1.g0000001"),
                offer: sample_offer(2, "0000002"),
                phase: InstallPhase::Verifying,
            },
            Event::Armed(Ok(())),
        );
        assert!(matches!(effect, Effect::None));
        assert_eq!(project(&next), super::super::Prompt::QuitToInstall);
    }

    #[test]
    fn the_same_build_on_recheck_is_the_download() {
        let offer = sample_offer(2, "0000002");
        let (_, effect) = step(
            Update::Installing {
                our: id("0.1.0.r1.g0000001"),
                offer: offer.clone(),
                phase: InstallPhase::Recheck,
            },
            Event::Feed(Ok(Feed {
                best: Some(offer.clone()),
            })),
        );
        match effect {
            Effect::Download { offer: got } => assert_eq!(got.build, offer.build),
            _ => panic!("the same build downloads"),
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

    fn scratch(label: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "thinwire-update-{label}-{}-{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn write_bundle(path: &Path, marker: &str) {
        let file = path.join("Contents/Resources/BUILD_ID");
        fs::create_dir_all(file.parent().expect("resources")).expect("resources");
        fs::write(file, marker).expect("marker");
    }

    fn read_marker(path: &Path) -> String {
        fs::read_to_string(path.join("Contents/Resources/BUILD_ID")).expect("marker")
    }

    #[test]
    fn converge_twice_leaves_the_new_bundle_at_dest() {
        let root = scratch("twice");
        let staged = root.join("staging/build/Thinwire.app");
        let parent = root.join("Applications");
        fs::create_dir_all(&parent).expect("parent");
        let dest = parent.join("Thinwire.app");
        let fresh = parent.join("Thinwire.app.new");
        let previous = parent.join("Thinwire.app.previous");
        write_bundle(&staged, "new");
        write_bundle(&dest, "old");
        assert_eq!(
            converge(&staged, &fresh, &dest, &previous).expect("first swap"),
            Swapped::Installed
        );
        assert_eq!(read_marker(&dest), "new");
        assert_eq!(read_marker(&previous), "old");
        assert!(!staged.exists());
        assert!(!fresh.exists());
        assert_eq!(
            converge(&staged, &fresh, &dest, &previous).expect("second swap"),
            Swapped::Installed
        );
        assert!(!previous.exists());
        assert_eq!(read_marker(&dest), "new");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn converge_resumes_when_only_fresh_and_previous_exist() {
        let root = scratch("resume");
        let staged = root.join("staging/Thinwire.app");
        let parent = root.join("Applications");
        fs::create_dir_all(&parent).expect("parent");
        let dest = parent.join("Thinwire.app");
        let fresh = parent.join("Thinwire.app.new");
        let previous = parent.join("Thinwire.app.previous");
        write_bundle(&fresh, "new");
        write_bundle(&previous, "old");
        assert_eq!(
            converge(&staged, &fresh, &dest, &previous).expect("resume"),
            Swapped::Installed
        );
        assert_eq!(read_marker(&dest), "new");
        assert!(!fresh.exists());
        assert_eq!(read_marker(&previous), "old");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn converge_replaces_a_partial_fresh_directory() {
        let root = scratch("partial");
        let staged = root.join("staging/Thinwire.app");
        let parent = root.join("Applications");
        fs::create_dir_all(&parent).expect("parent");
        let dest = parent.join("Thinwire.app");
        let fresh = parent.join("Thinwire.app.new");
        let previous = parent.join("Thinwire.app.previous");
        write_bundle(&staged, "new");
        write_bundle(&dest, "old");
        fs::create_dir_all(fresh.join("Contents")).expect("partial fresh");
        assert_eq!(
            converge(&staged, &fresh, &dest, &previous).expect("recover"),
            Swapped::Installed
        );
        assert_eq!(read_marker(&dest), "new");
        assert!(!fresh.exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_redirect_stays_on_the_asset_hosts() {
        let start = "https://github.com/jaysonsantos/thinwire/releases/download/nightly/thinwire-macos-arm64-0.1.0.r2.gabc1234.tar.gz";
        let next = follow_redirect(
            start,
            "https://release-assets.githubusercontent.com/a/b?token=1",
            0,
        )
        .expect("asset host");
        assert!(next.starts_with("https://release-assets.githubusercontent.com/"));
        assert!(follow_redirect(start, "https://evil.example/x", 0).is_err());
        assert!(follow_redirect(start, "http://objects.githubusercontent.com/x", 0).is_err());
        assert!(follow_redirect(start, "https://objects.githubusercontent.com/x", 3).is_err());
        assert!(follow_redirect(start, "https://github.com/other", 0).is_err());
        assert!(
            follow_redirect(&next, "https://github-releases.githubusercontent.com/z", 1).is_ok()
        );
    }

    #[test]
    fn developer_id_text_requires_team_identifier_and_authority() {
        let good = "\
Identifier=dev.jaysonsantos.thinwire
Authority=Developer ID Application: Example (89SP4VB8Y7)
TeamIdentifier=89SP4VB8Y7
";
        assert!(developer_id_text(good));
        assert!(!developer_id_text(
            "Identifier=dev.jaysonsantos.thinwire\nSignature=adhoc\n"
        ));
        assert!(!developer_id_text(
            "Identifier=dev.jaysonsantos.thinwire.extra\nAuthority=Developer ID Application: Example (89SP4VB8Y7)\nTeamIdentifier=89SP4VB8Y7\n"
        ));
        assert!(!developer_id_text(
            "Identifier=dev.jaysonsantos.thinwire\nAuthority=Developer ID Application: Example (89SP4VB8Y7)\nTeamIdentifier=AAAAAAAAAA\n"
        ));
    }

    fn gzip_tar(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut raw = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut raw);
            for (name, bytes) in entries {
                let mut header = tar::Header::new_gnu();
                if bytes.is_empty() && name.ends_with('/') {
                    header.set_entry_type(tar::EntryType::Directory);
                    header.set_size(0);
                } else {
                    header.set_entry_type(tar::EntryType::Regular);
                    header.set_size(bytes.len() as u64);
                    header.set_mode(0o755);
                }
                header.set_path(name).expect("path");
                header.set_cksum();
                builder.append(&header, *bytes).expect("entry");
            }
            builder.finish().expect("tar");
        }
        let mut encoded = Vec::new();
        {
            let mut gz = flate2::write::GzEncoder::new(&mut encoded, flate2::Compression::fast());
            gz.write_all(&raw).expect("gzip");
            gz.finish().expect("gzip finish");
        }
        encoded
    }

    fn gzip_bytes(raw: &[u8]) -> Vec<u8> {
        let mut encoded = Vec::new();
        let mut gz = flate2::write::GzEncoder::new(&mut encoded, flate2::Compression::fast());
        gz.write_all(raw).expect("gzip");
        gz.finish().expect("gzip finish");
        encoded
    }

    /// A ustar member the `tar` builder refuses to write, so the test can hand
    /// the extractor a path with `..`.
    fn raw_ustar(name: &str, bytes: &[u8]) -> Vec<u8> {
        let mut header = vec![0u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[100..108].copy_from_slice(b"0000644\0");
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");
        let size = format!("{:011o}\0", bytes.len());
        header[124..136].copy_from_slice(size.as_bytes());
        header[136..148].copy_from_slice(b"00000000000\0");
        header[156] = b'0';
        header[257..262].copy_from_slice(b"ustar");
        header[263..265].copy_from_slice(b"00");
        header[148..156].copy_from_slice(b"        ");
        let sum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
        let cksum = format!("{sum:06o}\0 ");
        header[148..156].copy_from_slice(cksum.as_bytes());
        header.extend_from_slice(bytes);
        let pad = (512 - (bytes.len() % 512)) % 512;
        header.extend(std::iter::repeat_n(0, pad));
        header
    }

    #[test]
    fn a_tar_that_escapes_is_not_extracted() {
        let root = scratch("escape");
        let archive = root.join("bad.tar.gz");
        let mut raw = raw_ustar("Thinwire.app/../../outside.txt", b"nope");
        raw.extend_from_slice(&[0u8; 1024]);
        fs::write(&archive, gzip_bytes(&raw)).expect("archive");
        let dest = root.join("out");
        assert_eq!(
            extract_archive(&archive, &dest).expect_err("escape"),
            BAD_SIGNATURE
        );
        assert!(!root.join("outside.txt").exists());
        assert!(!dest.join("outside.txt").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_link_that_leaves_the_bundle_is_rejected() {
        let bundle = Path::new("/Applications/Thinwire.app");
        let link = Path::new("Thinwire.app/Contents/MacOS/helper");
        assert!(!link_stays_inside(bundle, link, Path::new("/tmp/evil")));
        assert!(!link_stays_inside(
            bundle,
            link,
            Path::new("../../../outside")
        ));
        assert!(link_stays_inside(bundle, link, Path::new("thinwire")));
        assert!(safe_member(Path::new("/etc/passwd")).is_err());
        assert!(safe_member(Path::new("Thinwire.app/Contents/MacOS/thinwire")).is_ok());
    }

    #[test]
    fn a_bundle_tar_keeps_both_helpers() {
        let root = scratch("helpers");
        let archive = root.join("app.tar.gz");
        let bytes = gzip_tar(&[
            ("Thinwire.app/", b""),
            ("Thinwire.app/Contents/", b""),
            ("Thinwire.app/Contents/MacOS/", b""),
            ("Thinwire.app/Contents/MacOS/thinwire", b"app"),
            (
                "Thinwire.app/Contents/MacOS/thinwire-whatsapp-helper",
                b"wa",
            ),
            ("Thinwire.app/Contents/MacOS/thinwire-signal-helper", b"sg"),
        ]);
        fs::write(&archive, bytes).expect("archive");
        let dest = root.join("out");
        let bundle = extract_archive(&archive, &dest).expect("bundle");
        let macos = bundle.join("Contents/MacOS");
        assert!(macos.join("thinwire-whatsapp-helper").is_file());
        assert!(macos.join("thinwire-signal-helper").is_file());
        assert_eq!(fs::read(macos.join("thinwire")).expect("app bytes"), b"app");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_publish_script_signature_verifies() {
        let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/sign-update.sh");
        let dir = scratch("sign");
        let pem = dir.join("fixture.pem");
        let public = dir.join("pub.txt");
        let message = dir.join("appcast.xml");
        fs::write(&message, b"<rss>fixture</rss>\n").expect("message");
        let generated = std::process::Command::new("openssl")
            .args(["genpkey", "-algorithm", "ED25519", "-out"])
            .arg(&pem)
            .status()
            .expect("openssl");
        assert!(generated.success());
        let derived = std::process::Command::new("openssl")
            .args(["pkey", "-in"])
            .arg(&pem)
            .args(["-pubout", "-outform", "DER"])
            .output()
            .expect("public key");
        assert!(derived.status.success());
        let raw = derived.stdout[derived.stdout.len() - 32..].to_vec();
        fs::write(&public, format!("{}\n", STANDARD.encode(&raw))).expect("public file");
        let pem_text = fs::read_to_string(&pem).expect("pem");
        let signed = std::process::Command::new(&script)
            .arg(&message)
            .env("THINWIRE_UPDATE_KEY", &pem_text)
            .env("THINWIRE_UPDATE_PUBLIC_KEY_FILE", &public)
            .output()
            .expect("sign-update");
        let err = String::from_utf8_lossy(&signed.stderr);
        assert!(
            !err.contains("PRIVATE KEY"),
            "sign-update wrote a private key"
        );
        assert!(signed.status.success(), "sign-update failed");
        let sig_text = String::from_utf8(signed.stdout).expect("utf8");
        let sig_bytes = STANDARD.decode(sig_text.trim()).expect("base64");
        let signature: [u8; 64] = sig_bytes.try_into().expect("64 bytes");
        let key_bytes: [u8; 32] = raw.as_slice().try_into().expect("32 bytes");
        let key = ed25519_dalek::VerifyingKey::from_bytes(&key_bytes).expect("key");
        verify_detached(&fs::read(&message).expect("message"), &signature, &key)
            .expect("signature");

        fs::write(&public, "not-the-key\n").expect("wrong public");
        let refused = std::process::Command::new(&script)
            .arg(&message)
            .env("THINWIRE_UPDATE_KEY", &pem_text)
            .env("THINWIRE_UPDATE_PUBLIC_KEY_FILE", &public)
            .output()
            .expect("sign-update");
        let refused_err = String::from_utf8_lossy(&refused.stderr);
        assert!(
            !refused_err.contains("PRIVATE KEY"),
            "sign-update wrote a private key"
        );
        assert!(!refused.status.success());
        let _ = fs::remove_dir_all(&dir);
    }
}
