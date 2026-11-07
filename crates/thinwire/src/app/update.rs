//! A Windows build compiled on `main` of this repository can replace itself
//! from the `nightly` pre-release.
//!
//! The worker thread is the only writer of [`Phase`]. The window sends
//! commands. [`Ledger`] and [`Journal`] are the only records on disk. The
//! progress of each executable is not stored: [`observe`] reads the install
//! directory.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Seven lowercase hex characters. The `g` suffix of `VERSION`, and the
/// first seven characters of the commit this binary was compiled from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Sha7([u8; 7]);

impl Sha7 {
    fn parse(text: &str) -> Option<Self> {
        if text.len() != 7 {
            return None;
        }
        if !text
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        {
            return None;
        }
        let mut bytes = [0u8; 7];
        bytes.copy_from_slice(text.as_bytes());
        Some(Self(bytes))
    }

    fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).expect("sha7 is seven hex bytes")
    }
}

/// A parsed `VERSION` line. `rev` is the only order key. `token` is the
/// line, without one trailing newline, and is what the prompt shows.
#[derive(Clone, PartialEq, Eq, Debug)]
struct NightlyVersion {
    token: String,
    rev: u32,
    sha7: Sha7,
}

/// What Install downloads. The archive name is built from `version.token`.
/// It is not taken from a line of `SHA256SUMS`.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Offer {
    version: NightlyVersion,
    archive_sha256: [u8; 32],
}

/// The in-app update. The worker is the only writer.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Phase {
    Quiet,
    Checking,
    Offer(Offer),
    /// The archive is being hashed, unpacked, and copied beside the live
    /// names. Those names are not renamed yet.
    Fetching(Offer),
    /// The journal is on disk and the staged executable has been spawned.
    Handoff(Offer),
    Failed {
        error: PlainError,
        /// `Some` retries the download. `None` retries the check.
        retry: Option<Offer>,
    },
}

/// What the window draws. `Checking` is [`Snapshot::Quiet`].
#[derive(Clone, PartialEq, Eq, Debug)]
enum Snapshot {
    Quiet,
    Offer {
        version: String,
        notes: &'static str,
    },
    Busy {
        version: String,
    },
    Failed(PlainError),
}

/// Three lines: what happened, why, and what to do.
#[derive(Clone, PartialEq, Eq, Debug)]
struct PlainError {
    happened: String,
    why: String,
    next: String,
}

/// `%APPDATA%/thinwire/update/ledger.json`. `settings.toml` is not opened.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
struct Ledger {
    proven: Option<Proven>,
    deferred_rev: Option<u32>,
    swap_error: Option<PlainError>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct Proven {
    sha7: Sha7,
    rev: u32,
}

/// Durable handoff. The install directory is the parent of the running
/// app at Install time, not the parent of the staged copy.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Journal {
    target: NightlyVersion,
    install_dir: PathBuf,
    staging_dir: PathBuf,
}

/// One executable in the install directory, read from the files that are there.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Slot {
    /// Live name and `.incoming`. No `.old`.
    Ready,
    /// `.old` and `.incoming`. The live name is missing.
    Aside,
    /// Live name and `.old`. No `.incoming`.
    Committed,
    /// Only the live name.
    Idle,
    /// Any other combination. Do not invent a repair.
    Broken,
}

/// A tar member after one leading `./` is removed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Member<'a> {
    Exe(usize),
    License,
    /// `""` is the directory itself. Any other value is a path under it.
    Notices(&'a str),
}

/// What [`decide`] tells the worker to do with one feed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Decide {
    Quiet,
    /// This build is the published tip. The caller records [`Ledger::proven`].
    Current,
    Offer,
}

/// What a normal start does with a journal the previous process left behind.
/// The apply process does not use this. It always swaps.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum JournalAction {
    Cleanup,
    Resume,
}

/// Swap order, and the only executables an archive may contain.
const EXES: [&str; 3] = [
    "thinwire.exe",
    "thinwire-whatsapp-helper.exe",
    "thinwire-signal-helper.exe",
];

const NOTES: &str = "This is not a release.";

const OFFICIAL_REPOSITORY: &str = "jaysonsantos/thinwire";

const MAIN_REF: &str = "refs/heads/main";

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
const FEED_PREFIX: &str = "https://github.com/jaysonsantos/thinwire/releases/download/nightly/";

/// The feed hosts a redirect may land on. The `Location` target is not logged.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
const FEED_HOSTS: [&str; 2] = ["github.com", "release-assets.githubusercontent.com"];

/// The live `r277` archive is 48,729,593 bytes.
const MAX_ARCHIVE_BYTES: u64 = 128 * 1024 * 1024;

/// The live unpacked executables are about 124 MiB.
const MAX_UNPACKED_BYTES: u64 = 256 * 1024 * 1024;

const APPLY_ARG: &str = "--thinwire-apply";

/// From the apply child starting until it gives up on `instance.lock`.
const APPLY_WAIT: Duration = Duration::from_secs(15);

/// `VERSION` and `SHA256SUMS` are a few lines. A larger body is not the feed.
#[cfg(windows)]
const TEXT_CAP: u64 = 64 * 1024;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Identity of this binary. Callers only build it with [`Launch::compile_time`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct BuildStamp {
    sha7: Sha7,
}

/// Opaque to the window. A dev build, a fork, a pull request, and Linux or
/// macOS have no stamp, and the worker does not start.
pub(crate) struct Launch {
    stamp: Option<BuildStamp>,
}

impl Launch {
    /// `GITHUB_SHA`, `GITHUB_REPOSITORY`, and `GITHUB_REF` are read with
    /// `option_env`. A value exported in the shell after compile does not count.
    pub(crate) fn compile_time() -> Self {
        Self {
            stamp: stamp_from(
                cfg!(windows),
                option_env!("GITHUB_REPOSITORY"),
                option_env!("GITHUB_REF"),
                option_env!("GITHUB_SHA"),
            ),
        }
    }
}

fn stamp_from(
    windows: bool,
    repository: Option<&str>,
    reference: Option<&str>,
    sha: Option<&str>,
) -> Option<BuildStamp> {
    if !windows {
        return None;
    }
    if repository != Some(OFFICIAL_REPOSITORY) || reference != Some(MAIN_REF) {
        return None;
    }
    let sha = sha?;
    if sha.len() != 40 || Sha7::parse(&sha[..7]).is_none() {
        return None;
    }
    if !sha
        .bytes()
        .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return None;
    }
    Sha7::parse(&sha[..7]).map(|sha7| BuildStamp { sha7 })
}

fn check_failed() -> PlainError {
    PlainError {
        happened: "thinwire could not check for an update.".to_owned(),
        why: "The nightly feed did not answer.".to_owned(),
        next: "The installed app was not changed.".to_owned(),
    }
}

fn bad_payload() -> PlainError {
    PlainError {
        happened: "thinwire did not install this update.".to_owned(),
        why: "The download did not match the published checksum.".to_owned(),
        next: "The installed app was not changed.".to_owned(),
    }
}

fn swap_rolled_back() -> PlainError {
    PlainError {
        happened: "thinwire did not replace itself.".to_owned(),
        why: "Windows would not let the new files take the place of the old ones.".to_owned(),
        next: "The previous files were put back.".to_owned(),
    }
}

fn notes() -> &'static str {
    NOTES
}

/// `VERSION` matches the AUR script after one trailing `\n` is removed.
fn parse_version(text: &str) -> Result<NightlyVersion, PlainError> {
    let text = text.strip_suffix('\n').unwrap_or(text);
    let Some((head, sha)) = text.rsplit_once(".g") else {
        return Err(check_failed());
    };
    let Some(sha7) = Sha7::parse(sha) else {
        return Err(check_failed());
    };
    let Some((semver, rev_text)) = head.rsplit_once(".r") else {
        return Err(check_failed());
    };
    if !is_digits(rev_text) || !is_semver(semver) {
        return Err(check_failed());
    }
    let rev = rev_text.parse().map_err(|_| check_failed())?;
    Ok(NightlyVersion {
        token: text.to_owned(),
        rev,
        sha7,
    })
}

fn is_digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

fn is_semver(text: &str) -> bool {
    let mut parts = text.split('.');
    parts.next().is_some_and(is_digits)
        && parts.next().is_some_and(is_digits)
        && parts.next().is_some_and(is_digits)
        && parts.next().is_none()
}

fn archive_name(version: &NightlyVersion) -> String {
    format!("thinwire-windows-x86_64-{}.tar.gz", version.token)
}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn feed_url(name: &str) -> String {
    format!("{FEED_PREFIX}{name}")
}

/// The line whose name is the Windows archive. Zero matches or two matches
/// is an error. A filename in the file is not a URL.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn parse_sums(text: &str, version: &NightlyVersion) -> Result<[u8; 32], PlainError> {
    let want = archive_name(version);
    let mut found = None;
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        let Some((hash, name)) = split_sum_line(line) else {
            continue;
        };
        if name != want {
            continue;
        }
        let digest = parse_hex32(hash).ok_or_else(check_failed)?;
        if found.is_some() {
            return Err(check_failed());
        }
        found = Some(digest);
    }
    found.ok_or_else(check_failed)
}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn split_sum_line(line: &str) -> Option<(&str, &str)> {
    let mut parts = line.split_whitespace();
    let hash = parts.next()?;
    let name = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let name = name.strip_prefix('*').unwrap_or(name);
    if name.is_empty() {
        return None;
    }
    Some((hash, name))
}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn parse_hex32(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    let bytes = text.as_bytes();
    for (index, byte) in out.iter_mut().enumerate() {
        let hi = hex_value(bytes[index * 2])?;
        let lo = hex_value(bytes[index * 2 + 1])?;
        *byte = (hi << 4) | lo;
    }
    Some(out)
}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// One leading `./` is removed. `\` , a leading `/`, a drive prefix, and a
/// component that is empty, `.`, or `..` are rejected. The live archive
/// stores `./thinwire.exe`.
fn normalize_member(name: &str) -> Result<Option<Member<'_>>, PlainError> {
    if name.contains('\\') || name.contains(':') {
        return Err(bad_payload());
    }
    let mut rest = name.strip_prefix("./").unwrap_or(name);
    if rest.starts_with('/') {
        return Err(bad_payload());
    }
    let directory = rest.ends_with('/');
    if directory {
        rest = &rest[..rest.len() - 1];
    }
    if rest.is_empty() {
        return Ok(None);
    }
    if rest
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(bad_payload());
    }
    if let Some(index) = EXES.iter().position(|exe| *exe == rest) {
        if directory {
            return Err(bad_payload());
        }
        return Ok(Some(Member::Exe(index)));
    }
    if rest == "LICENSE" {
        if directory {
            return Err(bad_payload());
        }
        return Ok(Some(Member::License));
    }
    if rest == "THIRD_PARTY_NOTICES" {
        return Ok(Some(Member::Notices("")));
    }
    if let Some(path) = rest.strip_prefix("THIRD_PARTY_NOTICES/") {
        return Ok(Some(Member::Notices(path)));
    }
    Err(bad_payload())
}

fn decide(stamp: Option<&BuildStamp>, nightly: &NightlyVersion, ledger: &Ledger) -> Decide {
    let Some(stamp) = stamp else {
        return Decide::Quiet;
    };
    if stamp.sha7 == nightly.sha7 {
        return Decide::Current;
    }
    if ledger.deferred_rev == Some(nightly.rev) {
        return Decide::Quiet;
    }
    if let Some(proven) = &ledger.proven
        && proven.sha7 == stamp.sha7
        && nightly.rev <= proven.rev
    {
        return Decide::Quiet;
    }
    Decide::Offer
}

fn snapshot(phase: &Phase) -> Snapshot {
    match phase {
        Phase::Quiet | Phase::Checking => Snapshot::Quiet,
        Phase::Offer(offer) => Snapshot::Offer {
            version: offer.version.token.clone(),
            notes: notes(),
        },
        Phase::Fetching(offer) | Phase::Handoff(offer) => Snapshot::Busy {
            version: offer.version.token.clone(),
        },
        Phase::Failed { error, .. } => Snapshot::Failed(error.clone()),
    }
}

fn journal_action(stamp: Option<&BuildStamp>, journal: &Journal) -> JournalAction {
    match stamp {
        Some(stamp) if stamp.sha7 == journal.target.sha7 => JournalAction::Cleanup,
        _ => JournalAction::Resume,
    }
}

/// HTTPS, and the host is one of [`FEED_HOSTS`]. At most five hops is the
/// caller's limit. The target is not logged: its query is a signature.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn redirect_allowed(next: &str) -> bool {
    let Ok(url) = url::Url::parse(next) else {
        return false;
    };
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    if url.port().is_some_and(|port| port != 443) {
        return false;
    }
    url.host_str()
        .is_some_and(|host| FEED_HOSTS.contains(&host))
}

fn live_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(name)
}

fn incoming_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.incoming"))
}

fn old_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.old"))
}

fn observe(install_dir: &Path, name: &str) -> Slot {
    let live = live_path(install_dir, name).is_file();
    let incoming = incoming_path(install_dir, name).is_file();
    let old = old_path(install_dir, name).is_file();
    match (live, incoming, old) {
        (true, true, false) => Slot::Ready,
        (false, true, true) => Slot::Aside,
        (true, false, true) => Slot::Committed,
        (true, false, false) => Slot::Idle,
        _ => Slot::Broken,
    }
}

/// One step toward [`Slot::Committed`]. `Idle` and [`Slot::Broken`] fail
/// and leave the directory as it was.
fn advance(install_dir: &Path, name: &str) -> Result<Slot, PlainError> {
    match observe(install_dir, name) {
        Slot::Committed => Ok(Slot::Committed),
        Slot::Ready => {
            rename_file(&live_path(install_dir, name), &old_path(install_dir, name))?;
            rename_file(
                &incoming_path(install_dir, name),
                &live_path(install_dir, name),
            )?;
            expect_slot(install_dir, name, Slot::Committed)
        }
        Slot::Aside => {
            rename_file(
                &incoming_path(install_dir, name),
                &live_path(install_dir, name),
            )?;
            expect_slot(install_dir, name, Slot::Committed)
        }
        Slot::Idle | Slot::Broken => Err(swap_rolled_back()),
    }
}

/// One step toward [`Slot::Idle`]. A crash between the two renames is the
/// slot [`observe`] reports on the next call.
fn retreat(install_dir: &Path, name: &str) -> Result<Slot, PlainError> {
    match observe(install_dir, name) {
        Slot::Idle => Ok(Slot::Idle),
        Slot::Ready => {
            remove_file(&incoming_path(install_dir, name))?;
            expect_slot(install_dir, name, Slot::Idle)
        }
        Slot::Aside => {
            rename_file(&old_path(install_dir, name), &live_path(install_dir, name))?;
            // Incoming is still there, so this step stops at Ready.
            expect_slot(install_dir, name, Slot::Ready)
        }
        Slot::Committed => {
            let incoming = incoming_path(install_dir, name);
            if !incoming.exists() {
                rename_file(&live_path(install_dir, name), &incoming)?;
            }
            rename_file(&old_path(install_dir, name), &live_path(install_dir, name))?;
            expect_slot(install_dir, name, Slot::Ready)
        }
        Slot::Broken => Err(swap_rolled_back()),
    }
}

fn expect_slot(install_dir: &Path, name: &str, want: Slot) -> Result<Slot, PlainError> {
    let got = observe(install_dir, name);
    if got == want {
        Ok(got)
    } else {
        Err(swap_rolled_back())
    }
}

fn rename_file(from: &Path, to: &Path) -> Result<(), PlainError> {
    // A destination that is already there is not the slot we observed.
    // Replacing it would hide that, and on Windows a rename does not replace.
    if to.exists() {
        return Err(swap_rolled_back());
    }
    std::fs::rename(from, to).map_err(|_| swap_rolled_back())
}

fn remove_file(path: &Path) -> Result<(), PlainError> {
    std::fs::remove_file(path).map_err(|_| swap_rolled_back())
}

fn constant_time_eq(left: &[u8; 32], right: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for (left, right) in left.iter().zip(right.iter()) {
        diff |= left ^ right;
    }
    diff == 0
}

fn sha256_bytes(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// Hash, unpack, and copy `.incoming`. The live names stay. A bad digest or
/// a bad member deletes the staging directory and any `.incoming` just written.
fn stage_verified(
    bytes: &[u8],
    expected: &[u8; 32],
    journal_path: &Path,
    journal: &Journal,
) -> Result<(), PlainError> {
    let install = &journal.install_dir;
    let staging = &journal.staging_dir;
    if bytes.len() as u64 > MAX_ARCHIVE_BYTES || !constant_time_eq(&sha256_bytes(bytes), expected) {
        let _ = std::fs::remove_dir_all(staging);
        clear_incoming(install);
        return Err(bad_payload());
    }
    if let Err(error) = extract_archive(bytes, staging) {
        clear_incoming(install);
        return Err(error);
    }
    if let Err(error) = copy_incoming(staging, install) {
        clear_incoming(install);
        let _ = std::fs::remove_dir_all(staging);
        let _ = std::fs::remove_file(journal_path);
        return Err(error);
    }
    if let Err(error) = write_journal(journal_path, journal) {
        clear_incoming(install);
        let _ = std::fs::remove_dir_all(staging);
        let _ = std::fs::remove_file(journal_path);
        return Err(error);
    }
    Ok(())
}

fn extract_archive(bytes: &[u8], staging: &Path) -> Result<(), PlainError> {
    if staging.exists() {
        std::fs::remove_dir_all(staging).map_err(|_| bad_payload())?;
    }
    ensure_dir(staging).map_err(|_| bad_payload())?;
    let extracted = unpack_archive(bytes, staging);
    if extracted.is_err() {
        let _ = std::fs::remove_dir_all(staging);
    }
    extracted
}

fn unpack_archive(bytes: &[u8], staging: &Path) -> Result<(), PlainError> {
    let decoder = flate2::read::GzDecoder::new(std::io::Cursor::new(bytes));
    let mut archive = tar::Archive::new(decoder);
    let mut unpacked = 0u64;
    let mut seen = [false; EXES.len()];
    let entries = archive.entries().map_err(|_| bad_payload())?;
    for entry in entries {
        let mut entry = entry.map_err(|_| bad_payload())?;
        let kind = entry.header().entry_type();
        if kind.is_gnu_longname()
            || kind.is_gnu_longlink()
            || kind.is_pax_global_extensions()
            || kind.is_pax_local_extensions()
        {
            continue;
        }
        if kind.is_symlink() || kind.is_hard_link() || kind.is_gnu_sparse() {
            return Err(bad_payload());
        }
        let name = entry
            .path()
            .map_err(|_| bad_payload())?
            .to_str()
            .ok_or_else(bad_payload)?
            .to_owned();
        let Some(member) = normalize_member(&name)? else {
            continue;
        };
        if kind.is_dir() {
            let Member::Notices(relative) = member else {
                return Err(bad_payload());
            };
            ensure_dir(&notices_path(staging, relative)).map_err(|_| bad_payload())?;
            continue;
        }
        if !kind.is_file() {
            return Err(bad_payload());
        }
        let size = entry.size();
        if size == 0 && matches!(member, Member::Exe(_)) {
            return Err(bad_payload());
        }
        unpacked = unpacked.saturating_add(size);
        if unpacked > MAX_UNPACKED_BYTES {
            return Err(bad_payload());
        }
        let dest = member_path(staging, member)?;
        if dest.exists() {
            return Err(bad_payload());
        }
        if let Some(parent) = dest.parent() {
            ensure_dir(parent).map_err(|_| bad_payload())?;
        }
        let mut file = std::fs::File::create(&dest).map_err(|_| bad_payload())?;
        let copied = std::io::copy(&mut entry, &mut file).map_err(|_| bad_payload())?;
        if copied != size {
            return Err(bad_payload());
        }
        if let Member::Exe(index) = member {
            seen[index] = true;
        }
    }
    if seen != [true, true, true] {
        return Err(bad_payload());
    }
    Ok(())
}

fn member_path(staging: &Path, member: Member<'_>) -> Result<PathBuf, PlainError> {
    let dest = match member {
        Member::Exe(index) => staging.join(EXES[index]),
        Member::License => staging.join("LICENSE"),
        Member::Notices(relative) => notices_path(staging, relative),
    };
    if dest.starts_with(staging) {
        Ok(dest)
    } else {
        Err(bad_payload())
    }
}

fn notices_path(staging: &Path, relative: &str) -> PathBuf {
    let root = staging.join("THIRD_PARTY_NOTICES");
    if relative.is_empty() {
        root
    } else {
        root.join(relative)
    }
}

fn copy_incoming(staging: &Path, install: &Path) -> Result<(), PlainError> {
    for name in EXES {
        let src = staging.join(name);
        let part = incoming_part(install, name);
        let dest = incoming_path(install, name);
        let _ = std::fs::remove_file(&part);
        std::fs::copy(&src, &part).map_err(|_| bad_payload())?;
        // The part name is not a slot. A crash while copying must not look Ready.
        if dest.exists() {
            let _ = std::fs::remove_file(&part);
            return Err(bad_payload());
        }
        if std::fs::rename(&part, &dest).is_err() {
            let _ = std::fs::remove_file(&part);
            return Err(bad_payload());
        }
    }
    Ok(())
}

fn incoming_part(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.incoming.part"))
}

fn clear_incoming(install: &Path) {
    for name in EXES {
        let _ = std::fs::remove_file(incoming_path(install, name));
        let _ = std::fs::remove_file(incoming_part(install, name));
    }
}

/// Drive every executable to [`Slot::Committed`], reading the directory
/// before each step. All [`Slot::Idle`] means nothing was staged.
fn commit_all(install_dir: &Path) -> Result<(), PlainError> {
    let slots: Vec<Slot> = EXES.iter().map(|name| observe(install_dir, name)).collect();
    if slots.iter().all(|slot| *slot == Slot::Idle) {
        return Ok(());
    }
    if slots
        .iter()
        .any(|slot| *slot == Slot::Idle || *slot == Slot::Broken)
    {
        let _ = retreat_all(install_dir);
        return Err(swap_rolled_back());
    }
    for name in EXES {
        if let Err(error) = finish_one(install_dir, name, Slot::Committed) {
            let _ = retreat_all(install_dir);
            return Err(error);
        }
    }
    Ok(())
}

/// Drive every executable to [`Slot::Idle`]. [`Slot::Broken`] stops the walk.
fn retreat_all(install_dir: &Path) -> Result<(), PlainError> {
    for name in EXES {
        finish_one(install_dir, name, Slot::Idle)?;
    }
    Ok(())
}

fn finish_one(install_dir: &Path, name: &str, goal: Slot) -> Result<(), PlainError> {
    for _ in 0..4 {
        let slot = observe(install_dir, name);
        if slot == goal {
            return Ok(());
        }
        match (goal, slot) {
            (Slot::Committed, Slot::Ready | Slot::Aside) => {
                advance(install_dir, name)?;
            }
            (Slot::Idle, Slot::Ready | Slot::Aside | Slot::Committed) => {
                retreat(install_dir, name)?;
            }
            _ => return Err(swap_rolled_back()),
        }
    }
    Err(swap_rolled_back())
}

fn write_journal(path: &Path, journal: &Journal) -> Result<(), PlainError> {
    let install = path_text(&journal.install_dir)?;
    let staging = path_text(&journal.staging_dir)?;
    let body = format!(
        "{{\n  \"token\": {},\n  \"rev\": {},\n  \"sha7\": {},\n  \"install_dir\": {},\n  \"staging_dir\": {}\n}}\n",
        json_string(&journal.target.token),
        journal.target.rev,
        json_string(journal.target.sha7.as_str()),
        json_string(&install),
        json_string(&staging),
    );
    replace_file(path, &body).map_err(|_| bad_payload())
}

fn read_journal(path: &Path) -> Result<Journal, PlainError> {
    let text = std::fs::read_to_string(path).map_err(|_| bad_payload())?;
    parse_journal(&text)
}

fn parse_journal(text: &str) -> Result<Journal, PlainError> {
    let mut scan = Scan { text, at: 0 };
    let mut token = None;
    let mut rev = None;
    let mut sha = None;
    let mut install = None;
    let mut staging = None;
    scan.eat('{')?;
    loop {
        if scan.peek() == Some('}') {
            scan.eat('}')?;
            break;
        }
        let key = scan.string()?;
        scan.eat(':')?;
        match key.as_str() {
            "token" => token = Some(scan.string()?),
            "rev" => rev = Some(scan.u32()?),
            "sha7" => sha = Some(scan.string()?),
            "install_dir" => install = Some(scan.string()?),
            "staging_dir" => staging = Some(scan.string()?),
            _ => return Err(bad_payload()),
        }
        if scan.peek() == Some(',') {
            scan.eat(',')?;
            continue;
        }
        scan.eat('}')?;
        break;
    }
    scan.finish()?;
    let version = parse_version(&token.ok_or_else(bad_payload)?).map_err(|_| bad_payload())?;
    if version.rev != rev.ok_or_else(bad_payload)?
        || version.sha7.as_str() != sha.as_deref().unwrap_or("")
    {
        return Err(bad_payload());
    }
    let install_dir = PathBuf::from(install.ok_or_else(bad_payload)?);
    let staging_dir = PathBuf::from(staging.ok_or_else(bad_payload)?);
    if !install_dir.is_absolute() || !staging_dir.is_absolute() {
        return Err(bad_payload());
    }
    Ok(Journal {
        target: version,
        install_dir,
        staging_dir,
    })
}

fn load_ledger(path: &Path) -> Ledger {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ledger::default();
    };
    parse_ledger(&text).unwrap_or_default()
}

fn store_ledger(path: &Path, ledger: &Ledger) -> Result<(), PlainError> {
    let proven = match &ledger.proven {
        Some(proven) => format!(
            "{{\"sha7\": {}, \"rev\": {}}}",
            json_string(proven.sha7.as_str()),
            proven.rev
        ),
        None => "null".to_owned(),
    };
    let deferred = match ledger.deferred_rev {
        Some(rev) => rev.to_string(),
        None => "null".to_owned(),
    };
    let swap = match &ledger.swap_error {
        Some(error) => format!(
            "{{\"happened\": {}, \"why\": {}, \"next\": {}}}",
            json_string(&error.happened),
            json_string(&error.why),
            json_string(&error.next)
        ),
        None => "null".to_owned(),
    };
    let body = format!(
        "{{\n  \"proven\": {proven},\n  \"deferred_rev\": {deferred},\n  \"swap_error\": {swap}\n}}\n"
    );
    replace_file(path, &body).map_err(|_| bad_payload())
}

fn parse_ledger(text: &str) -> Result<Ledger, PlainError> {
    let mut scan = Scan { text, at: 0 };
    let mut proven = None;
    let mut deferred = None;
    let mut swap = None;
    let mut seen_proven = false;
    let mut seen_deferred = false;
    let mut seen_swap = false;
    scan.eat('{')?;
    loop {
        if scan.peek() == Some('}') {
            scan.eat('}')?;
            break;
        }
        let key = scan.string()?;
        scan.eat(':')?;
        match key.as_str() {
            "proven" => {
                proven = parse_proven(&mut scan)?;
                seen_proven = true;
            }
            "deferred_rev" => {
                deferred = parse_optional_u32(&mut scan)?;
                seen_deferred = true;
            }
            "swap_error" => {
                swap = parse_swap_error(&mut scan)?;
                seen_swap = true;
            }
            _ => return Err(bad_payload()),
        }
        if scan.peek() == Some(',') {
            scan.eat(',')?;
            continue;
        }
        scan.eat('}')?;
        break;
    }
    scan.finish()?;
    if !seen_proven || !seen_deferred || !seen_swap {
        return Err(bad_payload());
    }
    Ok(Ledger {
        proven,
        deferred_rev: deferred,
        swap_error: swap,
    })
}

fn parse_proven(scan: &mut Scan<'_>) -> Result<Option<Proven>, PlainError> {
    if scan.peek() == Some('n') {
        scan.null()?;
        return Ok(None);
    }
    scan.eat('{')?;
    let mut sha = None;
    let mut rev = None;
    loop {
        if scan.peek() == Some('}') {
            scan.eat('}')?;
            break;
        }
        let key = scan.string()?;
        scan.eat(':')?;
        match key.as_str() {
            "sha7" => sha = Some(scan.string()?),
            "rev" => rev = Some(scan.u32()?),
            _ => return Err(bad_payload()),
        }
        if scan.peek() == Some(',') {
            scan.eat(',')?;
            continue;
        }
        scan.eat('}')?;
        break;
    }
    let sha7 = Sha7::parse(&sha.ok_or_else(bad_payload)?).ok_or_else(bad_payload)?;
    Ok(Some(Proven {
        sha7,
        rev: rev.ok_or_else(bad_payload)?,
    }))
}

fn parse_optional_u32(scan: &mut Scan<'_>) -> Result<Option<u32>, PlainError> {
    if scan.peek() == Some('n') {
        scan.null()?;
        return Ok(None);
    }
    Ok(Some(scan.u32()?))
}

fn parse_swap_error(scan: &mut Scan<'_>) -> Result<Option<PlainError>, PlainError> {
    if scan.peek() == Some('n') {
        scan.null()?;
        return Ok(None);
    }
    scan.eat('{')?;
    let mut happened = None;
    let mut why = None;
    let mut next = None;
    loop {
        if scan.peek() == Some('}') {
            scan.eat('}')?;
            break;
        }
        let key = scan.string()?;
        scan.eat(':')?;
        match key.as_str() {
            "happened" => happened = Some(scan.string()?),
            "why" => why = Some(scan.string()?),
            "next" => next = Some(scan.string()?),
            _ => return Err(bad_payload()),
        }
        if scan.peek() == Some(',') {
            scan.eat(',')?;
            continue;
        }
        scan.eat('}')?;
        break;
    }
    Ok(Some(PlainError {
        happened: happened.ok_or_else(bad_payload)?,
        why: why.ok_or_else(bad_payload)?,
        next: next.ok_or_else(bad_payload)?,
    }))
}

fn path_text(path: &Path) -> Result<String, PlainError> {
    path.to_str()
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
        .ok_or_else(bad_payload)
}

fn json_string(text: &str) -> String {
    let mut out = String::from("\"");
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other if other.is_control() => {
                out.push_str(&format!("\\u{:04x}", u32::from(other)));
            }
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

struct Scan<'a> {
    text: &'a str,
    at: usize,
}

impl Scan<'_> {
    fn skip(&mut self) {
        while self
            .text
            .get(self.at..)
            .is_some_and(|rest| rest.starts_with(|ch: char| ch.is_ascii_whitespace()))
        {
            self.at += 1;
        }
    }

    fn peek(&mut self) -> Option<char> {
        self.skip();
        self.text[self.at..].chars().next()
    }

    fn eat(&mut self, expected: char) -> Result<(), PlainError> {
        self.skip();
        match self.text[self.at..].chars().next() {
            Some(ch) if ch == expected => {
                self.at += ch.len_utf8();
                Ok(())
            }
            _ => Err(bad_payload()),
        }
    }

    fn string(&mut self) -> Result<String, PlainError> {
        self.eat('"')?;
        let bytes = self.text.as_bytes();
        let mut out = String::new();
        loop {
            if self.at >= bytes.len() {
                return Err(bad_payload());
            }
            match bytes[self.at] {
                b'"' => {
                    self.at += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.at += 1;
                    let esc = *bytes.get(self.at).ok_or_else(bad_payload)?;
                    self.at += 1;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hex = bytes.get(self.at..self.at + 4).ok_or_else(bad_payload)?;
                            let hex = std::str::from_utf8(hex).map_err(|_| bad_payload())?;
                            let code = u32::from_str_radix(hex, 16).map_err(|_| bad_payload())?;
                            out.push(char::from_u32(code).ok_or_else(bad_payload)?);
                            self.at += 4;
                        }
                        _ => return Err(bad_payload()),
                    }
                }
                byte if byte < 0x20 => return Err(bad_payload()),
                _ => {
                    let ch = self.text[self.at..]
                        .chars()
                        .next()
                        .ok_or_else(bad_payload)?;
                    out.push(ch);
                    self.at += ch.len_utf8();
                }
            }
        }
    }

    fn u32(&mut self) -> Result<u32, PlainError> {
        self.skip();
        let start = self.at;
        while self
            .text
            .get(self.at..)
            .is_some_and(|rest| rest.starts_with(|ch: char| ch.is_ascii_digit()))
        {
            self.at += 1;
        }
        if start == self.at {
            return Err(bad_payload());
        }
        self.text[start..self.at].parse().map_err(|_| bad_payload())
    }

    fn null(&mut self) -> Result<(), PlainError> {
        self.skip();
        if self.text[self.at..].starts_with("null") {
            self.at += 4;
            Ok(())
        } else {
            Err(bad_payload())
        }
    }

    fn finish(&mut self) -> Result<(), PlainError> {
        self.skip();
        if self.at == self.text.len() {
            Ok(())
        } else {
            Err(bad_payload())
        }
    }
}

fn replace_file(path: &Path, text: &str) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "the file has no directory",
        )
    })?;
    ensure_dir(parent)?;
    let temp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("update"),
        std::process::id()
    ));
    let _ = std::fs::remove_file(&temp);
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
    }
    // Windows rename does not replace an existing file. The temp file is
    // already durable, so removing the previous record and renaming is the
    // crash window, and a missing record is read as empty.
    if std::fs::rename(&temp, path).is_err() {
        let _ = std::fs::remove_file(path);
        if let Err(error) = std::fs::rename(&temp, path) {
            let _ = std::fs::remove_file(&temp);
            return Err(error);
        }
    }
    Ok(())
}

fn ensure_dir(dir: &Path) -> std::io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// The window sends these. It never writes [`Phase`].
#[derive(Debug)]
enum Command {
    Install,
    Later,
    Dismiss,
    Again,
}

#[derive(Debug)]
enum Step {
    Stay,
    Quiet,
    Check,
    Fetch(Offer),
}

#[derive(Debug)]
enum SwapEnd {
    Installed,
    Idle,
}

#[derive(Debug)]
enum Argv {
    Normal,
    /// The flag was present. `None` means the path was missing or extra
    /// arguments followed it, so the install directory is left alone.
    Apply(Option<PathBuf>),
}

/// The UI thread's handle. The worker owns every write to [`Phase`].
pub(crate) struct Updates {
    phase: Arc<Mutex<Phase>>,
    commands: Sender<Command>,
    shutdown: Arc<AtomicBool>,
}

impl Updates {
    /// No thread when `launch` has no stamp. `wake` only repaints.
    pub(crate) fn spawn(launch: Launch, wake: impl Fn() + Send + Sync + 'static) -> Self {
        let phase = Arc::new(Mutex::new(Phase::Quiet));
        let (commands, incoming) = std::sync::mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        if let Some(stamp) = launch.stamp {
            let phase_worker = Arc::clone(&phase);
            let shutdown_worker = Arc::clone(&shutdown);
            if std::thread::Builder::new()
                .name("thinwire-update".to_owned())
                .spawn(move || worker(stamp, phase_worker, incoming, shutdown_worker, wake))
                .is_err()
            {
                tracing::warn!("the update check did not start");
            }
        }
        Self {
            phase,
            commands,
            shutdown,
        }
    }

    /// One `true`. A later `Close` would hit `CloseGate::Waiting` and cancel.
    pub(crate) fn take_shutdown(&self) -> bool {
        self.shutdown.swap(false, Ordering::SeqCst)
    }

    pub(crate) fn consume_escape(&self, ctx: &egui::Context) {
        let pressed = ctx.input(|input| input.key_pressed(egui::Key::Escape));
        if !pressed {
            return;
        }
        let phase = lock_phase(&self.phase).clone();
        let command = match &phase {
            Phase::Offer(_) => Some(Command::Later),
            Phase::Failed { .. } => Some(Command::Dismiss),
            Phase::Fetching(_) | Phase::Handoff(_) => None,
            Phase::Quiet | Phase::Checking => return,
        };
        let _ = ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        if let Some(command) = command {
            let _ = self.commands.send(command);
        }
    }

    pub(crate) fn draw(&self, ctx: &egui::Context) {
        let phase = lock_phase(&self.phase).clone();
        match snapshot(&phase) {
            Snapshot::Quiet => {}
            Snapshot::Offer { .. } => {
                if let Phase::Offer(offer) = &phase {
                    draw_offer(ctx, offer, &self.commands);
                }
            }
            Snapshot::Busy { .. } => match &phase {
                Phase::Fetching(offer) | Phase::Handoff(offer) => draw_busy(ctx, offer),
                Phase::Quiet | Phase::Checking | Phase::Offer(_) | Phase::Failed { .. } => {}
            },
            Snapshot::Failed(error) => {
                let retry_download = matches!(phase, Phase::Failed { retry: Some(_), .. });
                draw_failed(ctx, &error, retry_download, &self.commands);
            }
        }
    }
}

fn lock_phase(phase: &Mutex<Phase>) -> std::sync::MutexGuard<'_, Phase> {
    phase.lock().unwrap_or_else(PoisonError::into_inner)
}

fn set_phase(phase: &Mutex<Phase>, next: Phase, wake: &impl Fn()) {
    *lock_phase(phase) = next;
    wake();
}

fn draw_offer(ctx: &egui::Context, offer: &Offer, commands: &Sender<Command>) {
    let mut install = false;
    let mut later = false;
    let modal = egui::Modal::new(prompt_id()).show(ctx, |ui| {
        ui.set_width(320.0);
        ui.label(egui::RichText::new(&offer.version.token).strong());
        ui.add_space(super::theme::space::S);
        ui.label(notes());
        ui.add_space(super::theme::space::M);
        ui.horizontal(|ui| {
            if ui.button("Install").clicked() {
                install = true;
            }
            if ui.button("Later").clicked() {
                later = true;
            }
        });
    });
    if install {
        let _ = commands.send(Command::Install);
    } else if later || modal.should_close() {
        let _ = commands.send(Command::Later);
    }
}

fn draw_busy(ctx: &egui::Context, offer: &Offer) {
    egui::Modal::new(prompt_id()).show(ctx, |ui| {
        ui.set_width(320.0);
        ui.label(egui::RichText::new(&offer.version.token).strong());
        ui.add_space(super::theme::space::S);
        ui.label("Installing…");
    });
}

fn draw_failed(
    ctx: &egui::Context,
    error: &PlainError,
    retry_download: bool,
    commands: &Sender<Command>,
) {
    let mut again = false;
    let mut dismiss = false;
    let modal = egui::Modal::new(prompt_id()).show(ctx, |ui| {
        let palette = super::theme::palette(ui);
        ui.set_width(360.0);
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.label(
            egui::RichText::new(format!("What happened: {}", error.happened))
                .strong()
                .color(palette.error),
        );
        ui.label(egui::RichText::new(format!("Why: {}", error.why)).color(palette.text2));
        ui.label(egui::RichText::new(format!("What to do: {}", error.next)).color(palette.text2));
        ui.add_space(super::theme::space::M);
        ui.horizontal(|ui| {
            if ui.button("Try again").clicked() {
                again = true;
            }
            if ui.button("Dismiss").clicked() {
                dismiss = true;
            }
        });
    });
    if again {
        let command = if retry_download {
            Command::Install
        } else {
            Command::Again
        };
        let _ = commands.send(command);
    } else if dismiss || modal.should_close() {
        let _ = commands.send(Command::Dismiss);
    }
}

fn prompt_id() -> egui::Id {
    egui::Id::unique("thinwire-update")
}

fn worker(
    stamp: BuildStamp,
    phase: Arc<Mutex<Phase>>,
    commands: Receiver<Command>,
    shutdown: Arc<AtomicBool>,
    wake: impl Fn(),
) {
    let Some(ledger_file) = ledger_file() else {
        set_phase(
            &phase,
            Phase::Failed {
                error: check_failed(),
                retry: None,
            },
            &wake,
        );
        return;
    };
    let mut ledger = load_ledger(&ledger_file);
    settle_startup(&stamp, &phase, &shutdown, &wake, &mut ledger, &ledger_file);
    while let Ok(command) = commands.recv() {
        match apply_command(&lock_phase(&phase).clone(), command, &mut ledger) {
            Step::Stay => {}
            Step::Quiet => {
                let _ = store_ledger(&ledger_file, &ledger);
                set_phase(&phase, Phase::Quiet, &wake);
            }
            Step::Check => {
                set_phase(&phase, Phase::Checking, &wake);
                publish_check(&stamp, &phase, &mut ledger, &ledger_file, &wake);
            }
            Step::Fetch(offer) => {
                set_phase(&phase, Phase::Fetching(offer.clone()), &wake);
                match install_offer(&offer) {
                    Ok(()) => {
                        shutdown.store(true, Ordering::SeqCst);
                        set_phase(&phase, Phase::Handoff(offer), &wake);
                    }
                    Err(error) => set_phase(
                        &phase,
                        Phase::Failed {
                            error,
                            retry: Some(offer),
                        },
                        &wake,
                    ),
                }
            }
        }
    }
}

fn apply_command(phase: &Phase, command: Command, ledger: &mut Ledger) -> Step {
    match (phase, command) {
        (Phase::Offer(offer), Command::Install)
        | (
            Phase::Failed {
                retry: Some(offer), ..
            },
            Command::Install,
        ) => Step::Fetch(offer.clone()),
        (Phase::Offer(offer), Command::Later) => {
            ledger.deferred_rev = Some(offer.version.rev);
            Step::Quiet
        }
        (Phase::Failed { .. }, Command::Dismiss) => {
            ledger.swap_error = None;
            Step::Quiet
        }
        (Phase::Failed { retry: None, .. }, Command::Again) => Step::Check,
        _ => Step::Stay,
    }
}

fn settle_startup(
    stamp: &BuildStamp,
    phase: &Mutex<Phase>,
    shutdown: &AtomicBool,
    wake: &impl Fn(),
    ledger: &mut Ledger,
    ledger_file: &Path,
) {
    let Some(journal_file) = journal_file() else {
        publish_check(stamp, phase, ledger, ledger_file, wake);
        return;
    };
    if !journal_file.is_file() {
        if let Some(install) = install_dir() {
            clear_incoming(&install);
        }
        publish_check(stamp, phase, ledger, ledger_file, wake);
        return;
    }
    let Ok(journal) = read_journal(&journal_file) else {
        let _ = std::fs::remove_file(&journal_file);
        publish_check(stamp, phase, ledger, ledger_file, wake);
        return;
    };
    match journal_action(Some(stamp), &journal) {
        JournalAction::Cleanup => {
            cleanup_swap(&journal, &journal_file, ledger_file);
            set_phase(phase, Phase::Quiet, wake);
        }
        JournalAction::Resume => {
            if journal.staging_dir.join(EXES[0]).is_file() {
                match spawn_apply(&journal.staging_dir, &journal_file) {
                    Ok(()) => {
                        shutdown.store(true, Ordering::SeqCst);
                        set_phase(phase, Phase::Handoff(offer_for_resume(&journal)), wake);
                    }
                    Err(error) => {
                        clear_incoming(&journal.install_dir);
                        let _ = std::fs::remove_file(&journal_file);
                        set_phase(phase, Phase::Failed { error, retry: None }, wake);
                    }
                }
            } else {
                let _ = std::fs::remove_file(&journal_file);
                set_phase(
                    phase,
                    Phase::Failed {
                        error: swap_rolled_back(),
                        retry: None,
                    },
                    wake,
                );
            }
        }
    }
}

fn offer_for_resume(journal: &Journal) -> Offer {
    // The digest was checked before the journal existed. The card shows the token.
    Offer {
        version: journal.target.clone(),
        archive_sha256: [0; 32],
    }
}

fn cleanup_swap(journal: &Journal, journal_file: &Path, ledger_file: &Path) {
    let mut ledger = load_ledger(ledger_file);
    ledger.proven = Some(Proven {
        sha7: journal.target.sha7,
        rev: journal.target.rev,
    });
    ledger.swap_error = None;
    if ledger.deferred_rev == Some(journal.target.rev) {
        ledger.deferred_rev = None;
    }
    let _ = store_ledger(ledger_file, &ledger);
    for name in EXES {
        let _ = std::fs::remove_file(old_path(&journal.install_dir, name));
        let _ = std::fs::remove_file(incoming_path(&journal.install_dir, name));
        let _ = std::fs::remove_file(incoming_part(&journal.install_dir, name));
    }
    let _ = std::fs::remove_file(journal_file);
    let _ = std::fs::remove_dir_all(&journal.staging_dir);
}

fn publish_check(
    stamp: &BuildStamp,
    phase: &Mutex<Phase>,
    ledger: &mut Ledger,
    ledger_file: &Path,
    wake: &impl Fn(),
) {
    let swap = ledger.swap_error.clone();
    match fetch_offer() {
        Err(error) => {
            set_phase(
                phase,
                Phase::Failed {
                    error: swap.unwrap_or(error),
                    retry: None,
                },
                wake,
            );
        }
        Ok(offer) => match decide(Some(stamp), &offer.version, ledger) {
            Decide::Current => {
                record_current(ledger, &offer.version);
                let _ = store_ledger(ledger_file, ledger);
                if let Some(error) = swap {
                    set_phase(phase, Phase::Failed { error, retry: None }, wake);
                } else {
                    set_phase(phase, Phase::Quiet, wake);
                }
            }
            Decide::Quiet => {
                if let Some(error) = swap {
                    set_phase(phase, Phase::Failed { error, retry: None }, wake);
                } else {
                    set_phase(phase, Phase::Quiet, wake);
                }
            }
            Decide::Offer => {
                if let Some(error) = swap {
                    set_phase(phase, Phase::Failed { error, retry: None }, wake);
                } else {
                    set_phase(phase, Phase::Offer(offer), wake);
                }
            }
        },
    }
}

fn record_current(ledger: &mut Ledger, nightly: &NightlyVersion) {
    ledger.proven = Some(Proven {
        sha7: nightly.sha7,
        rev: nightly.rev,
    });
    if ledger.deferred_rev == Some(nightly.rev) {
        ledger.deferred_rev = None;
    }
}

fn install_offer(offer: &Offer) -> Result<(), PlainError> {
    let install = install_dir().ok_or_else(bad_payload)?;
    let root = update_root().ok_or_else(bad_payload)?;
    let staging = root.join("staging");
    let journal_path = root.join("journal.json");
    let bytes = download_archive(&archive_name(&offer.version))?;
    let journal = Journal {
        target: offer.version.clone(),
        install_dir: install.clone(),
        staging_dir: staging.clone(),
    };
    stage_verified(&bytes, &offer.archive_sha256, &journal_path, &journal)?;
    if let Err(error) = spawn_apply(&staging, &journal_path) {
        clear_incoming(&install);
        let _ = std::fs::remove_file(&journal_path);
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }
    Ok(())
}

fn fetch_offer() -> Result<Offer, PlainError> {
    #[cfg(windows)]
    {
        fetch_offer_windows()
    }
    #[cfg(not(windows))]
    {
        Err(check_failed())
    }
}

fn download_archive(name: &str) -> Result<Vec<u8>, PlainError> {
    #[cfg(windows)]
    {
        http_get(&feed_url(name), MAX_ARCHIVE_BYTES).map_err(|_| bad_payload())
    }
    #[cfg(not(windows))]
    {
        let _ = name;
        Err(bad_payload())
    }
}

#[cfg(windows)]
fn fetch_offer_windows() -> Result<Offer, PlainError> {
    let version = parse_version(&String::from_utf8_lossy(&http_get(
        &feed_url("VERSION"),
        TEXT_CAP,
    )?))?;
    let sums = String::from_utf8_lossy(&http_get(&feed_url("SHA256SUMS"), TEXT_CAP)?).into_owned();
    let archive_sha256 = parse_sums(&sums, &version)?;
    Ok(Offer {
        version,
        archive_sha256,
    })
}

#[cfg(windows)]
fn http_get(start: &str, max_body: u64) -> Result<Vec<u8>, PlainError> {
    let agent = ureq::Agent::config_builder()
        .https_only(true)
        .max_redirects(0)
        .timeout_global(Some(Duration::from_secs(60)))
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::NativeTls)
                .build(),
        )
        .build()
        .new_agent();
    let mut url = start.to_owned();
    let mut redirects = 0u32;
    loop {
        let response = agent.get(&url).call().map_err(|_| check_failed())?;
        let status = response.status().as_u16();
        tracing::info!(url = start, status, "nightly feed");
        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            if redirects == 5 {
                return Err(check_failed());
            }
            redirects += 1;
            let location = response
                .headers()
                .get("location")
                .and_then(|value| value.to_str().ok())
                .ok_or_else(check_failed)?;
            let next = resolve_redirect(&url, location)?;
            if !redirect_allowed(&next) {
                return Err(check_failed());
            }
            url = next;
            continue;
        }
        if status != 200 {
            return Err(check_failed());
        }
        return read_body(response, max_body);
    }
}

#[cfg(windows)]
fn read_body(
    mut response: ureq::http::Response<ureq::Body>,
    max_body: u64,
) -> Result<Vec<u8>, PlainError> {
    use std::io::Read;
    let mut reader = response.body_mut().as_reader();
    let mut out = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    loop {
        let read = reader.read(&mut buf).map_err(|_| check_failed())?;
        if read == 0 {
            break;
        }
        if out.len() as u64 + read as u64 > max_body {
            return Err(check_failed());
        }
        out.extend_from_slice(&buf[..read]);
    }
    Ok(out)
}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn resolve_redirect(current: &str, location: &str) -> Result<String, PlainError> {
    let base = url::Url::parse(current).map_err(|_| check_failed())?;
    base.join(location)
        .map(|next| next.to_string())
        .map_err(|_| check_failed())
}

/// `true` when this process is the apply child and has finished or refused.
/// A normal start returns `false`.
pub(crate) fn apply_if_requested() -> bool {
    match classify_argv(std::env::args().skip(1)) {
        Argv::Normal => false,
        Argv::Apply(None) => true,
        Argv::Apply(Some(path)) => {
            apply_from(&path);
            true
        }
    }
}

fn classify_argv(args: impl IntoIterator<Item = String>) -> Argv {
    let mut args = args.into_iter();
    if args.next().as_deref() != Some(APPLY_ARG) {
        return Argv::Normal;
    }
    let path = args.next().map(PathBuf::from);
    if args.next().is_some() {
        return Argv::Apply(None);
    }
    Argv::Apply(path)
}

fn apply_from(given: &Path) {
    let Some(root) = update_root() else {
        return;
    };
    let Some(_apply_lock) = wait_file_lock(&root.join("apply.lock"), APPLY_WAIT + APPLY_WAIT)
    else {
        return;
    };
    let expected = root.join("journal.json");
    let (Ok(given), Ok(expected_canon)) = (given.canonicalize(), expected.canonicalize()) else {
        return;
    };
    if given != expected_canon {
        return;
    }
    let Ok(journal) = read_journal(&expected) else {
        let _ = std::fs::remove_file(&expected);
        return;
    };
    let Some(instance_path) = thinwire_core::instance::InstanceLock::default_path() else {
        abandon_before_rename(&journal, &expected);
        return;
    };
    let Ok(_instance) = wait_instance(&instance_path) else {
        abandon_before_rename(&journal, &expected);
        return;
    };
    let ledger = ledger_file().unwrap_or_else(|| root.join("ledger.json"));
    match swap_files(&journal, &ledger, &expected) {
        Ok(SwapEnd::Installed) => {
            drop(_instance);
            let _ = spawn_normal(&journal.install_dir);
            for name in EXES {
                let _ = std::fs::remove_file(old_path(&journal.install_dir, name));
            }
            let _ = std::fs::remove_dir_all(&journal.staging_dir);
        }
        Ok(SwapEnd::Idle) => {
            drop(_instance);
            let _ = spawn_normal(&journal.install_dir);
        }
        Err(_) => {
            drop(_instance);
            let _ = spawn_normal(&journal.install_dir);
        }
    }
}

fn abandon_before_rename(journal: &Journal, journal_file: &Path) {
    clear_incoming(&journal.install_dir);
    let _ = std::fs::remove_file(journal_file);
    let _ = std::fs::remove_dir_all(&journal.staging_dir);
    if let Some(ledger_file) = ledger_file() {
        let mut ledger = load_ledger(&ledger_file);
        ledger.swap_error = Some(swap_rolled_back());
        let _ = store_ledger(&ledger_file, &ledger);
    }
}

fn swap_files(
    journal: &Journal,
    ledger_file: &Path,
    journal_file: &Path,
) -> Result<SwapEnd, PlainError> {
    match commit_all(&journal.install_dir) {
        Ok(()) => {
            let idle = EXES
                .iter()
                .all(|name| observe(&journal.install_dir, name) == Slot::Idle);
            if idle {
                let _ = std::fs::remove_file(journal_file);
                let _ = std::fs::remove_dir_all(&journal.staging_dir);
                return Ok(SwapEnd::Idle);
            }
            copy_notices(journal);
            let mut ledger = load_ledger(ledger_file);
            ledger.proven = Some(Proven {
                sha7: journal.target.sha7,
                rev: journal.target.rev,
            });
            ledger.swap_error = None;
            if ledger.deferred_rev == Some(journal.target.rev) {
                ledger.deferred_rev = None;
            }
            let _ = store_ledger(ledger_file, &ledger);
            let _ = std::fs::remove_file(journal_file);
            Ok(SwapEnd::Installed)
        }
        Err(error) => {
            clear_incoming(&journal.install_dir);
            let _ = std::fs::remove_file(journal_file);
            let _ = std::fs::remove_dir_all(&journal.staging_dir);
            let mut ledger = load_ledger(ledger_file);
            ledger.swap_error = Some(error.clone());
            let _ = store_ledger(ledger_file, &ledger);
            Err(error)
        }
    }
}

fn copy_notices(journal: &Journal) {
    let license = journal.staging_dir.join("LICENSE");
    if license.is_file() && std::fs::copy(&license, journal.install_dir.join("LICENSE")).is_err() {
        tracing::warn!("the new executables are in place and LICENSE was not copied");
    }
    let from = journal.staging_dir.join("THIRD_PARTY_NOTICES");
    if from.is_dir() && copy_tree(&from, &journal.install_dir.join("THIRD_PARTY_NOTICES")).is_err()
    {
        tracing::warn!("the new executables are in place and the notices were not copied");
    }
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "link"));
        }
        let dest = to.join(entry.file_name());
        if kind.is_dir() {
            copy_tree(&entry.path(), &dest)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), dest)?;
        }
    }
    Ok(())
}

fn wait_instance(path: &Path) -> Result<thinwire_core::instance::InstanceLock, ()> {
    wait_file_lock(path, APPLY_WAIT).ok_or(())
}

fn wait_file_lock(path: &Path, limit: Duration) -> Option<thinwire_core::instance::InstanceLock> {
    let deadline = Instant::now() + limit;
    loop {
        match thinwire_core::instance::InstanceLock::try_acquire(path) {
            Ok(lock) => return Some(lock),
            Err(thinwire_core::instance::LockError::Held) if Instant::now() < deadline => {
                let left = deadline.saturating_duration_since(Instant::now());
                std::thread::sleep(thinwire_core::instance::RETRY_STEP.min(left));
            }
            Err(_) => return None,
        }
    }
}

fn spawn_apply(staging: &Path, journal: &Path) -> Result<(), PlainError> {
    let mut command = std::process::Command::new(staging.join(EXES[0]));
    command
        .arg(APPLY_ARG)
        .arg(journal)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    // Waiting would deadlock: the child blocks on the instance lock this
    // process still holds, and this process would be waiting for the child.
    command.spawn().map(|_| ()).map_err(|_| bad_payload())
}

fn spawn_normal(install: &Path) -> Result<(), PlainError> {
    std::process::Command::new(install.join(EXES[0]))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|_| swap_rolled_back())
}

fn install_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
}

fn update_root() -> Option<PathBuf> {
    dirs::data_dir().map(|root| root.join("thinwire").join("update"))
}

fn ledger_file() -> Option<PathBuf> {
    update_root().map(|root| root.join("ledger.json"))
}

fn journal_file() -> Option<PathBuf> {
    update_root().map(|root| root.join("journal.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0.1.0.r277.g0c07a9c";

    fn version() -> NightlyVersion {
        parse_version(TOKEN).expect("real nightly token")
    }

    fn stamp_of(sha7: &str) -> BuildStamp {
        BuildStamp {
            sha7: Sha7::parse(sha7).expect("sha7"),
        }
    }

    fn digest(byte: u8) -> [u8; 32] {
        [byte; 32]
    }

    fn offer() -> Offer {
        Offer {
            version: version(),
            archive_sha256: digest(1),
        }
    }

    fn check_text() -> PlainError {
        PlainError {
            happened: "thinwire could not check for an update.".to_owned(),
            why: "The nightly feed did not answer.".to_owned(),
            next: "The installed app was not changed.".to_owned(),
        }
    }

    fn payload_text() -> PlainError {
        PlainError {
            happened: "thinwire did not install this update.".to_owned(),
            why: "The download did not match the published checksum.".to_owned(),
            next: "The installed app was not changed.".to_owned(),
        }
    }

    fn swap_text() -> PlainError {
        PlainError {
            happened: "thinwire did not replace itself.".to_owned(),
            why: "Windows would not let the new files take the place of the old ones.".to_owned(),
            next: "The previous files were put back.".to_owned(),
        }
    }

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos());
            let path = std::env::temp_dir().join(format!(
                "thinwire-update-{name}-{}-{nanos}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("scratch");
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_real_nightly_token_parses_with_one_trailing_newline() {
        let parsed = parse_version("0.1.0.r277.g0c07a9c\n").expect("token");
        assert_eq!(parsed.token, TOKEN);
        assert_eq!(parsed.rev, 277);
        assert_eq!(parsed.sha7.as_str(), "0c07a9c");
        assert_eq!(parse_version(TOKEN).expect("no newline").rev, 277);
    }

    #[test]
    fn a_bad_version_line_is_the_check_error() {
        for text in [
            "0.1.0.r277.g0c07a9c\n\n",
            "0.1.0.r277.g0c07a9C",
            "0.1.0.r277.g0c07a9",
            "0.1.r277.g0c07a9c",
            "0.1.0.r.g0c07a9c",
            "0.1.0.r277.g0c07a9c ",
            "../0.1.0.r277.g0c07a9c",
            "",
        ] {
            assert_eq!(parse_version(text).unwrap_err(), check_text(), "{text}");
        }
    }

    #[test]
    fn the_windows_archive_name_comes_from_the_token() {
        let name = archive_name(&version());
        assert_eq!(name, "thinwire-windows-x86_64-0.1.0.r277.g0c07a9c.tar.gz");
        assert_eq!(
            feed_url(&name),
            "https://github.com/jaysonsantos/thinwire/releases/download/nightly/thinwire-windows-x86_64-0.1.0.r277.g0c07a9c.tar.gz"
        );
        assert_eq!(
            feed_url("VERSION"),
            "https://github.com/jaysonsantos/thinwire/releases/download/nightly/VERSION"
        );
        assert_eq!(
            feed_url("SHA256SUMS"),
            "https://github.com/jaysonsantos/thinwire/releases/download/nightly/SHA256SUMS"
        );
    }

    #[test]
    fn sums_accept_gnu_text_and_a_star_and_ignore_another_archive() {
        let windows = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        let linux = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let name = archive_name(&version());
        let text = format!("{linux}  thinwire-linux-x86_64-{TOKEN}.tar.gz\n{windows}  {name}\n");
        assert_eq!(
            parse_sums(&text, &version()).expect("windows line"),
            parse_hex32(windows).expect("hex")
        );
        let starred = format!("{windows} *{name}\n");
        assert_eq!(
            parse_sums(&starred, &version()).expect("star"),
            parse_hex32(windows).expect("hex")
        );
        let spaced = format!("{windows}   {name}\n");
        assert_eq!(
            parse_sums(&spaced, &version()).expect("spaces"),
            parse_hex32(windows).expect("hex")
        );
    }

    #[test]
    fn a_sums_file_whose_first_line_is_not_windows_fails_when_windows_is_absent() {
        let linux = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let text = format!("{linux}  thinwire-linux-x86_64-{TOKEN}.tar.gz\n");
        assert_eq!(parse_sums(&text, &version()).unwrap_err(), check_text());
        let twice = format!(
            "{linux}  {}\n{linux}  {}\n",
            archive_name(&version()),
            archive_name(&version())
        );
        assert_eq!(parse_sums(&twice, &version()).unwrap_err(), check_text());
    }

    #[test]
    fn no_stamp_same_sha_a_deferral_and_an_older_feed_stay_quiet() {
        let nightly = version();
        let ours = stamp_of("aaaaaaa");
        let tip = stamp_of("0c07a9c");
        assert_eq!(decide(None, &nightly, &Ledger::default()), Decide::Quiet);
        assert_eq!(
            decide(Some(&tip), &nightly, &Ledger::default()),
            Decide::Current
        );
        let deferred = Ledger {
            deferred_rev: Some(277),
            ..Ledger::default()
        };
        assert_eq!(decide(Some(&ours), &nightly, &deferred), Decide::Quiet);
        // The same sha is the tip even when that revision was deferred.
        assert_eq!(decide(Some(&tip), &nightly, &deferred), Decide::Current);

        let proven = Ledger {
            proven: Some(Proven {
                sha7: ours.sha7,
                rev: 300,
            }),
            ..Ledger::default()
        };
        assert_eq!(
            decide(Some(&ours), &nightly, &proven),
            Decide::Quiet,
            "a feed rev below proven does not offer"
        );
        let same = Ledger {
            proven: Some(Proven {
                sha7: ours.sha7,
                rev: 277,
            }),
            ..Ledger::default()
        };
        assert_eq!(decide(Some(&ours), &nightly, &same), Decide::Quiet);
        let older = Ledger {
            proven: Some(Proven {
                sha7: ours.sha7,
                rev: 200,
            }),
            ..Ledger::default()
        };
        assert_eq!(decide(Some(&ours), &nightly, &older), Decide::Offer);
        let other = Ledger {
            proven: Some(Proven {
                sha7: stamp_of("bbbbbbb").sha7,
                rev: 900,
            }),
            ..Ledger::default()
        };
        assert_eq!(decide(Some(&ours), &nightly, &other), Decide::Offer);
        assert_eq!(
            decide(Some(&ours), &nightly, &Ledger::default()),
            Decide::Offer
        );
    }

    #[test]
    fn a_dot_slash_member_is_one_of_the_three_exes_or_the_notices() {
        assert_eq!(
            normalize_member("./thinwire.exe").expect("exe"),
            Some(Member::Exe(0))
        );
        assert_eq!(
            normalize_member("./thinwire-whatsapp-helper.exe").expect("wa"),
            Some(Member::Exe(1))
        );
        assert_eq!(
            normalize_member("./thinwire-signal-helper.exe").expect("signal"),
            Some(Member::Exe(2))
        );
        assert_eq!(
            normalize_member("thinwire.exe").expect("bare"),
            Some(Member::Exe(0))
        );
        assert_eq!(
            normalize_member("./LICENSE").expect("license"),
            Some(Member::License)
        );
        assert_eq!(
            normalize_member("./THIRD_PARTY_NOTICES").expect("dir"),
            Some(Member::Notices(""))
        );
        assert_eq!(
            normalize_member("./THIRD_PARTY_NOTICES/").expect("dir slash"),
            Some(Member::Notices(""))
        );
        assert_eq!(
            normalize_member("./THIRD_PARTY_NOTICES/tdlib-LICENSE_1_0.txt").expect("notice"),
            Some(Member::Notices("tdlib-LICENSE_1_0.txt"))
        );
        assert_eq!(normalize_member("./").expect("root"), None);
        assert_eq!(normalize_member("").expect("empty"), None);
    }

    #[test]
    fn a_member_named_dot_dot_thinwire_exe_is_refused() {
        for name in [
            "../thinwire.exe",
            "./../thinwire.exe",
            "/thinwire.exe",
            "\\\\thinwire.exe",
            "C:/thinwire.exe",
            "thinwire.exe:stream",
            "././thinwire.exe",
            "./thinwire.exe/",
            "THIRD_PARTY_NOTICES/../thinwire.exe",
            "./not-thinwire.exe",
            "./LICENSE/extra",
        ] {
            assert_eq!(
                normalize_member(name).unwrap_err(),
                payload_text(),
                "{name}"
            );
        }
    }

    #[test]
    fn ready_advances_to_committed_and_retreat_puts_the_old_bytes_back() {
        let scratch = Scratch::new("swap");
        let name = EXES[0];
        let live = live_path(&scratch.0, name);
        let incoming = incoming_path(&scratch.0, name);
        let old = old_path(&scratch.0, name);
        std::fs::write(&live, b"old").expect("live");
        std::fs::write(&incoming, b"new").expect("incoming");
        assert_eq!(observe(&scratch.0, name), Slot::Ready);
        assert_eq!(advance(&scratch.0, name).expect("advance"), Slot::Committed);
        assert_eq!(std::fs::read(&live).expect("live"), b"new");
        assert_eq!(std::fs::read(&old).expect("old"), b"old");
        assert!(!incoming.exists());
        assert_eq!(advance(&scratch.0, name).expect("again"), Slot::Committed);

        assert_eq!(retreat(&scratch.0, name).expect("one step"), Slot::Ready);
        assert_eq!(std::fs::read(&live).expect("restored live"), b"old");
        assert_eq!(std::fs::read(&incoming).expect("parked new"), b"new");
        assert!(!old.exists());
        assert_eq!(retreat(&scratch.0, name).expect("idle"), Slot::Idle);
        assert_eq!(std::fs::read(&live).expect("still old"), b"old");
        assert!(!incoming.exists());
        assert_eq!(retreat(&scratch.0, name).expect("still"), Slot::Idle);
    }

    #[test]
    fn aside_is_the_crash_between_the_two_renames_and_advance_finishes_it() {
        let scratch = Scratch::new("aside");
        let name = EXES[1];
        std::fs::write(old_path(&scratch.0, name), b"old").expect("old");
        std::fs::write(incoming_path(&scratch.0, name), b"new").expect("incoming");
        assert_eq!(observe(&scratch.0, name), Slot::Aside);
        assert_eq!(advance(&scratch.0, name).expect("finish"), Slot::Committed);
        assert_eq!(
            std::fs::read(live_path(&scratch.0, name)).expect("live"),
            b"new"
        );
        assert_eq!(
            std::fs::read(old_path(&scratch.0, name)).expect("old"),
            b"old"
        );
        assert!(!incoming_path(&scratch.0, name).exists());

        assert_eq!(retreat(&scratch.0, name).expect("back"), Slot::Ready);
        assert_eq!(
            retreat(&scratch.0, name).expect("idle"),
            Slot::Idle,
            "a second retreat drops the new bytes"
        );
        assert_eq!(
            std::fs::read(live_path(&scratch.0, name)).expect("original"),
            b"old"
        );
    }

    #[test]
    fn retreat_from_aside_restores_the_live_name_before_dropping_incoming() {
        let scratch = Scratch::new("aside-back");
        let name = EXES[2];
        std::fs::write(old_path(&scratch.0, name), b"old").expect("old");
        std::fs::write(incoming_path(&scratch.0, name), b"new").expect("incoming");
        assert_eq!(retreat(&scratch.0, name).expect("ready"), Slot::Ready);
        assert_eq!(
            std::fs::read(live_path(&scratch.0, name)).expect("live"),
            b"old"
        );
        assert!(incoming_path(&scratch.0, name).is_file());
    }

    #[test]
    fn an_idle_exe_is_not_advanced() {
        let scratch = Scratch::new("idle");
        let name = EXES[0];
        std::fs::write(live_path(&scratch.0, name), b"stay").expect("live");
        assert_eq!(observe(&scratch.0, name), Slot::Idle);
        assert_eq!(advance(&scratch.0, name).unwrap_err(), swap_text());
        assert_eq!(
            std::fs::read(live_path(&scratch.0, name)).expect("live"),
            b"stay"
        );
        assert!(!incoming_path(&scratch.0, name).exists());
        assert!(!old_path(&scratch.0, name).exists());
    }

    #[test]
    fn a_broken_slot_fails_and_the_files_stay() {
        let scratch = Scratch::new("broken");
        let name = EXES[0];
        std::fs::write(live_path(&scratch.0, name), b"live").expect("live");
        std::fs::write(incoming_path(&scratch.0, name), b"new").expect("incoming");
        std::fs::write(old_path(&scratch.0, name), b"old").expect("old");
        assert_eq!(observe(&scratch.0, name), Slot::Broken);
        assert_eq!(advance(&scratch.0, name).unwrap_err(), swap_text());
        assert_eq!(retreat(&scratch.0, name).unwrap_err(), swap_text());
        assert_eq!(
            std::fs::read(live_path(&scratch.0, name)).expect("live"),
            b"live"
        );
        assert_eq!(
            std::fs::read(incoming_path(&scratch.0, name)).expect("in"),
            b"new"
        );
        assert_eq!(
            std::fs::read(old_path(&scratch.0, name)).expect("old"),
            b"old"
        );

        let empty = Scratch::new("empty");
        assert_eq!(observe(&empty.0, name), Slot::Broken);
        assert_eq!(advance(&empty.0, name).unwrap_err(), swap_text());
        let only_new = Scratch::new("only-new");
        std::fs::write(incoming_path(&only_new.0, name), b"new").expect("incoming");
        assert_eq!(observe(&only_new.0, name), Slot::Broken);
        assert!(incoming_path(&only_new.0, name).is_file());
    }

    #[test]
    fn the_windows_allow_list_names_each_installed_exe() {
        let script = include_str!("../../../../scripts/stage-os-artifact.sh");
        let error = script
            .lines()
            .find(|line| line.contains("Windows payload must be only"))
            .expect("allow-list error");
        let helpers = script
            .lines()
            .find(|line| line.trim_start().starts_with("helper_names=("))
            .expect("helper names");
        let printed = error
            .replace("${helper_names[0]}", "thinwire-whatsapp-helper")
            .replace("${helper_names[1]}", "thinwire-signal-helper");
        assert!(
            helpers.contains("thinwire-whatsapp-helper"),
            "the allow list lost the WhatsApp helper"
        );
        assert!(
            helpers.contains("thinwire-signal-helper"),
            "the allow list lost the Signal helper"
        );
        for name in EXES {
            assert!(
                printed.contains(name),
                "{name} is missing from the Windows allow-list error: {printed}"
            );
        }
    }

    #[test]
    fn the_installed_exe_list_is_the_swap_order() {
        assert_eq!(
            EXES,
            [
                "thinwire.exe",
                "thinwire-whatsapp-helper.exe",
                "thinwire-signal-helper.exe",
            ]
        );
    }

    #[test]
    fn a_stamp_exists_only_for_windows_main_of_this_repository() {
        let sha = "0c07a9c0c07a9c0c07a9c0c07a9c0c07a9c0c07a";
        assert_eq!(sha.len(), 40);
        assert!(
            stamp_from(false, Some(OFFICIAL_REPOSITORY), Some(MAIN_REF), Some(sha)).is_none(),
            "Linux and macOS do not carry a stamp"
        );
        assert!(stamp_from(true, Some("other/thinwire"), Some(MAIN_REF), Some(sha)).is_none());
        assert!(
            stamp_from(
                true,
                Some(OFFICIAL_REPOSITORY),
                Some("refs/pull/1/merge"),
                Some(sha)
            )
            .is_none()
        );
        assert!(stamp_from(true, Some(OFFICIAL_REPOSITORY), Some(MAIN_REF), None).is_none());
        assert!(
            stamp_from(
                true,
                Some(OFFICIAL_REPOSITORY),
                Some(MAIN_REF),
                Some("0c07a9c")
            )
            .is_none()
        );
        assert!(
            stamp_from(
                true,
                Some(OFFICIAL_REPOSITORY),
                Some(MAIN_REF),
                Some("0C07A9C0C07A9C0C07A9C0C07A9C0C07A9C0C07A")
            )
            .is_none()
        );
        let stamp = stamp_from(true, Some(OFFICIAL_REPOSITORY), Some(MAIN_REF), Some(sha))
            .expect("official windows main");
        assert_eq!(stamp.sha7.as_str(), "0c07a9c");
        assert_eq!(
            Launch::compile_time().stamp,
            stamp_from(
                cfg!(windows),
                option_env!("GITHUB_REPOSITORY"),
                option_env!("GITHUB_REF"),
                option_env!("GITHUB_SHA"),
            )
        );
    }

    #[test]
    fn a_redirect_stays_on_https_and_the_two_feed_hosts() {
        assert!(redirect_allowed(
            "https://github.com/jaysonsantos/thinwire/releases/download/nightly/VERSION"
        ));
        assert!(redirect_allowed(
            "https://release-assets.githubusercontent.com/github-production-release-asset/1"
        ));
        assert!(!redirect_allowed("http://github.com/jaysonsantos/thinwire"));
        assert!(!redirect_allowed("https://example.com/thinwire.exe"));
        assert!(!redirect_allowed("https://github.com.example/VERSION"));
        assert!(!redirect_allowed("https://user@github.com/VERSION"));
        assert!(!redirect_allowed("https://github.com:8443/VERSION"));
        assert!(!redirect_allowed("/VERSION"));
        assert!(!redirect_allowed(""));
    }

    #[test]
    fn a_journal_for_our_sha_is_cleanup_and_any_other_is_resume() {
        let target = version();
        let journal = Journal {
            target: target.clone(),
            install_dir: PathBuf::from("C:/thinwire"),
            staging_dir: PathBuf::from("C:/data/update/staging"),
        };
        assert_eq!(
            journal_action(Some(&stamp_of(target.sha7.as_str())), &journal),
            JournalAction::Cleanup
        );
        assert_eq!(
            journal_action(Some(&stamp_of("aaaaaaa")), &journal),
            JournalAction::Resume
        );
        assert_eq!(journal_action(None, &journal), JournalAction::Resume);
    }

    #[test]
    fn the_prompt_snapshot_hides_a_check_and_shows_the_token() {
        assert!(matches!(snapshot(&Phase::Quiet), Snapshot::Quiet));
        assert!(matches!(snapshot(&Phase::Checking), Snapshot::Quiet));
        match snapshot(&Phase::Offer(offer())) {
            Snapshot::Offer { version, notes } => {
                assert_eq!(version, TOKEN);
                assert_eq!(notes, "This is not a release.");
            }
            other => panic!("offer snapshot was {other:?}"),
        }
        assert!(matches!(
            snapshot(&Phase::Fetching(offer())),
            Snapshot::Busy { version } if version == TOKEN
        ));
        assert!(matches!(
            snapshot(&Phase::Handoff(offer())),
            Snapshot::Busy { version } if version == TOKEN
        ));
        let error = payload_text();
        assert_eq!(
            snapshot(&Phase::Failed {
                error: error.clone(),
                retry: Some(offer()),
            }),
            Snapshot::Failed(error)
        );
    }

    struct Layout {
        _scratch: Scratch,
        install: PathBuf,
        staging: PathBuf,
        journal: PathBuf,
    }

    impl Layout {
        fn new(name: &str) -> Self {
            let scratch = Scratch::new(name);
            let install = scratch.0.join("install");
            let staging = scratch.0.join("staging");
            let journal = scratch.0.join("update").join("journal.json");
            std::fs::create_dir_all(&install).expect("install");
            for exe in EXES {
                std::fs::write(install.join(exe), format!("old-{exe}")).expect("live");
            }
            Self {
                _scratch: scratch,
                install,
                staging,
                journal,
            }
        }

        fn record(&self) -> Journal {
            Journal {
                target: version(),
                install_dir: self.install.clone(),
                staging_dir: self.staging.clone(),
            }
        }

        fn scratch_has(&self, name: &str) -> bool {
            self._scratch.0.join(name).exists()
        }
    }

    fn good_archive() -> (Vec<u8>, [u8; 32]) {
        let mut raw = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut raw);
            for exe in EXES {
                let bytes = format!("new-{exe}");
                let mut header = tar::Header::new_gnu();
                header.set_mode(0o755);
                header.set_size(bytes.len() as u64);
                header.set_cksum();
                builder
                    .append_data(&mut header, format!("./{exe}"), bytes.as_bytes())
                    .expect("member");
            }
            let mut header = tar::Header::new_gnu();
            header.set_mode(0o644);
            header.set_size(4);
            header.set_cksum();
            builder
                .append_data(&mut header, "./LICENSE", &b"mit\n"[..])
                .expect("license");
            let mut dir = tar::Header::new_gnu();
            dir.set_entry_type(tar::EntryType::Directory);
            dir.set_size(0);
            dir.set_mode(0o755);
            dir.set_cksum();
            builder
                .append_data(&mut dir, "./THIRD_PARTY_NOTICES", std::io::empty())
                .expect("notices dir");
            let mut notice = tar::Header::new_gnu();
            notice.set_mode(0o644);
            notice.set_size(3);
            notice.set_cksum();
            builder
                .append_data(&mut notice, "./THIRD_PARTY_NOTICES/note.txt", &b"ok\n"[..])
                .expect("notice");
            builder.finish().expect("tar");
        }
        let mut gz = Vec::new();
        {
            use std::io::Write;
            let mut enc = flate2::write::GzEncoder::new(&mut gz, flate2::Compression::fast());
            enc.write_all(&raw).expect("gzip");
            enc.finish().expect("gzip end");
        }
        let digest = sha256_bytes(&gz);
        (gz, digest)
    }

    fn ustar_header(name: &str, size: u64) -> [u8; 512] {
        let mut header = [0u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[100..108].copy_from_slice(b"0000755\0");
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");
        let size_field = format!("{size:011o}\0");
        header[124..136].copy_from_slice(size_field.as_bytes());
        header[136..148].copy_from_slice(b"00000000000\0");
        header[156] = b'0';
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        for byte in &mut header[148..156] {
            *byte = b' ';
        }
        let sum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
        let checksum = format!("{sum:06o}\0 ");
        header[148..156].copy_from_slice(checksum.as_bytes());
        header
    }

    #[test]
    fn a_wrong_digest_does_not_leave_incoming() {
        let layout = Layout::new("digest");
        let (bytes, digest) = good_archive();
        let mut wrong = digest;
        wrong[31] ^= 0xff;
        assert_eq!(wrong[..31], digest[..31], "only the last byte differs");
        let error = stage_verified(&bytes, &wrong, &layout.journal, &layout.record()).unwrap_err();
        assert_eq!(error, payload_text());
        for exe in EXES {
            assert_eq!(
                std::fs::read(layout.install.join(exe)).expect("live"),
                format!("old-{exe}").into_bytes()
            );
            assert!(!incoming_path(&layout.install, exe).exists());
            assert!(!incoming_part(&layout.install, exe).exists());
        }
        assert!(!layout.staging.exists());
        assert!(!layout.journal.exists());
    }

    #[test]
    fn a_hostile_member_does_not_replace_the_installed_exes() {
        let layout = Layout::new("hostile");
        let body = b"pwn\n";
        let mut raw = ustar_header("../thinwire.exe", body.len() as u64).to_vec();
        raw.extend_from_slice(body);
        raw.extend(std::iter::repeat_n(0u8, 512 - body.len()));
        let mut bytes = Vec::new();
        {
            use std::io::Write;
            let mut enc = flate2::write::GzEncoder::new(&mut bytes, flate2::Compression::fast());
            enc.write_all(&raw).expect("gzip");
            enc.finish().expect("gzip end");
        }
        let digest = sha256_bytes(&bytes);
        assert_eq!(
            stage_verified(&bytes, &digest, &layout.journal, &layout.record()).unwrap_err(),
            payload_text()
        );
        assert_eq!(
            std::fs::read(layout.install.join("thinwire.exe")).expect("live"),
            b"old-thinwire.exe"
        );
        assert!(!layout.scratch_has("thinwire.exe"));
        assert!(!incoming_path(&layout.install, "thinwire.exe").exists());
        assert!(!layout.staging.exists());
    }

    #[test]
    fn a_verified_archive_stages_incoming_and_the_journal_without_touching_live_exes() {
        let layout = Layout::new("stage");
        let (bytes, digest) = good_archive();
        stage_verified(&bytes, &digest, &layout.journal, &layout.record()).expect("stage");
        for exe in EXES {
            assert_eq!(
                std::fs::read(layout.install.join(exe)).expect("live"),
                format!("old-{exe}").into_bytes()
            );
            assert_eq!(
                std::fs::read(incoming_path(&layout.install, exe)).expect("incoming"),
                format!("new-{exe}").into_bytes()
            );
        }
        assert_eq!(
            std::fs::read(layout.staging.join("LICENSE")).expect("license"),
            b"mit\n"
        );
        assert_eq!(
            std::fs::read(layout.staging.join("THIRD_PARTY_NOTICES").join("note.txt"))
                .expect("note"),
            b"ok\n"
        );
        let journal = read_journal(&layout.journal).expect("journal");
        assert_eq!(journal.target.token, TOKEN);
        assert_eq!(journal.install_dir, layout.install);
        assert_eq!(journal.staging_dir, layout.staging);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(layout.journal.parent().expect("dir"))
                .expect("dir")
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "the update directory is private: {mode:o}");
        }
    }

    #[test]
    fn commit_swaps_the_bytes_and_retreat_puts_them_back() {
        let layout = Layout::new("commit");
        let (bytes, digest) = good_archive();
        stage_verified(&bytes, &digest, &layout.journal, &layout.record()).expect("stage");
        commit_all(&layout.install).expect("commit");
        for exe in EXES {
            assert_eq!(observe(&layout.install, exe), Slot::Committed);
            assert_eq!(
                std::fs::read(layout.install.join(exe)).expect("live"),
                format!("new-{exe}").into_bytes()
            );
            assert_eq!(
                std::fs::read(old_path(&layout.install, exe)).expect("old"),
                format!("old-{exe}").into_bytes()
            );
            assert!(!incoming_path(&layout.install, exe).exists());
        }
        retreat_all(&layout.install).expect("retreat");
        for exe in EXES {
            assert_eq!(observe(&layout.install, exe), Slot::Idle);
            assert_eq!(
                std::fs::read(layout.install.join(exe)).expect("restored"),
                format!("old-{exe}").into_bytes()
            );
            assert!(!incoming_path(&layout.install, exe).exists());
            assert!(!old_path(&layout.install, exe).exists());
        }
    }

    #[test]
    fn a_failed_incoming_copy_leaves_the_live_exes() {
        let layout = Layout::new("copy");
        std::fs::create_dir(incoming_path(&layout.install, EXES[2])).expect("block");
        let (bytes, digest) = good_archive();
        assert_eq!(
            stage_verified(&bytes, &digest, &layout.journal, &layout.record()).unwrap_err(),
            payload_text()
        );
        for exe in EXES {
            assert_eq!(
                std::fs::read(layout.install.join(exe)).expect("live"),
                format!("old-{exe}").into_bytes()
            );
        }
        assert!(!incoming_path(&layout.install, EXES[0]).exists());
        assert!(!incoming_path(&layout.install, EXES[1]).exists());
        assert!(!layout.journal.exists());
        assert!(!layout.staging.exists());
    }

    #[test]
    fn an_unpacked_member_over_the_cap_is_refused() {
        let layout = Layout::new("cap");
        let header = ustar_header("./thinwire.exe", MAX_UNPACKED_BYTES + 1);
        let mut raw = header.to_vec();
        raw.extend_from_slice(&[0u8; 512]);
        let mut bytes = Vec::new();
        {
            use std::io::Write;
            let mut enc = flate2::write::GzEncoder::new(&mut bytes, flate2::Compression::fast());
            enc.write_all(&raw).expect("gzip");
            enc.finish().expect("gzip end");
        }
        assert!(
            bytes.len() < 1024,
            "the cap test does not send a giant file"
        );
        let digest = sha256_bytes(&bytes);
        assert_eq!(
            stage_verified(&bytes, &digest, &layout.journal, &layout.record()).unwrap_err(),
            payload_text()
        );
        assert_eq!(
            std::fs::read(layout.install.join("thinwire.exe")).expect("live"),
            b"old-thinwire.exe"
        );
        assert!(!layout.staging.exists());
        assert!(!incoming_path(&layout.install, "thinwire.exe").exists());
    }

    #[test]
    fn the_journal_keeps_a_backslash_and_a_quote_in_the_path() {
        let layout = Layout::new("json");
        let journal = Journal {
            target: version(),
            install_dir: layout.install.join("quote\"and\\slash"),
            staging_dir: layout.staging.clone(),
        };
        std::fs::create_dir_all(&journal.install_dir).expect("dir");
        write_journal(&layout.journal, &journal).expect("write");
        assert_eq!(read_journal(&layout.journal).expect("read"), journal);
        assert!(
            std::fs::read_to_string(&layout.journal)
                .expect("text")
                .contains("quote\\\"and\\\\slash")
        );
    }

    #[test]
    fn a_corrupt_ledger_is_empty_and_a_good_one_roundtrips() {
        let scratch = Scratch::new("ledger");
        let path = scratch.0.join("ledger.json");
        assert_eq!(load_ledger(&path), Ledger::default());
        std::fs::write(&path, b"{not json").expect("corrupt");
        assert_eq!(load_ledger(&path), Ledger::default());
        let ledger = Ledger {
            proven: Some(Proven {
                sha7: Sha7::parse("0c07a9c").expect("sha"),
                rev: 277,
            }),
            deferred_rev: Some(12),
            swap_error: Some(swap_text()),
        };
        store_ledger(&path, &ledger).expect("store");
        assert_eq!(load_ledger(&path), ledger);
    }

    #[test]
    fn commit_of_an_idle_folder_does_nothing_and_a_broken_slot_rolls_back() {
        let layout = Layout::new("idle");
        commit_all(&layout.install).expect("nothing staged");
        for exe in EXES {
            assert_eq!(
                std::fs::read(layout.install.join(exe)).expect("live"),
                format!("old-{exe}").into_bytes()
            );
        }
        std::fs::write(incoming_path(&layout.install, EXES[0]), b"new").expect("incoming");
        std::fs::write(old_path(&layout.install, EXES[0]), b"older").expect("old");
        assert_eq!(observe(&layout.install, EXES[0]), Slot::Broken);
        assert_eq!(commit_all(&layout.install).unwrap_err(), swap_text());
        assert_eq!(
            std::fs::read(layout.install.join(EXES[0])).expect("live"),
            b"old-thinwire.exe"
        );
        assert!(incoming_path(&layout.install, EXES[0]).is_file());
        assert!(old_path(&layout.install, EXES[0]).is_file());
    }

    #[test]
    fn later_records_the_revision_and_try_again_retries_the_right_step() {
        let mut ledger = Ledger::default();
        let offered = Phase::Offer(offer());
        assert!(matches!(
            apply_command(&offered, Command::Install, &mut ledger),
            Step::Fetch(_)
        ));
        assert!(matches!(
            apply_command(&offered, Command::Later, &mut ledger),
            Step::Quiet
        ));
        assert_eq!(ledger.deferred_rev, Some(277));
        let failed_check = Phase::Failed {
            error: check_text(),
            retry: None,
        };
        assert!(matches!(
            apply_command(&failed_check, Command::Again, &mut ledger),
            Step::Check
        ));
        ledger.swap_error = Some(swap_text());
        assert!(matches!(
            apply_command(&failed_check, Command::Dismiss, &mut ledger),
            Step::Quiet
        ));
        assert!(ledger.swap_error.is_none());
        let failed_download = Phase::Failed {
            error: payload_text(),
            retry: Some(offer()),
        };
        assert!(matches!(
            apply_command(&failed_download, Command::Install, &mut ledger),
            Step::Fetch(_)
        ));
        assert!(matches!(
            apply_command(&Phase::Fetching(offer()), Command::Install, &mut ledger),
            Step::Stay
        ));
        assert!(matches!(
            apply_command(&Phase::Handoff(offer()), Command::Install, &mut ledger),
            Step::Stay
        ));
    }

    #[test]
    fn argv_without_the_apply_flag_is_a_normal_start() {
        assert!(matches!(classify_argv(std::iter::empty()), Argv::Normal));
        assert!(matches!(classify_argv(["--help".to_owned()]), Argv::Normal));
        assert!(matches!(
            classify_argv([APPLY_ARG.to_owned()]),
            Argv::Apply(None)
        ));
        assert!(matches!(
            classify_argv([
                APPLY_ARG.to_owned(),
                "journal.json".to_owned(),
                "extra".to_owned(),
            ]),
            Argv::Apply(None)
        ));
        match classify_argv([APPLY_ARG.to_owned(), "/tmp/journal.json".to_owned()]) {
            Argv::Apply(Some(path)) => assert_eq!(path, PathBuf::from("/tmp/journal.json")),
            other => panic!("expected the journal path, got {other:?}"),
        }
    }

    #[test]
    fn the_prompt_is_after_the_shell_and_escape_is_before_it() {
        let app = include_str!("mod.rs");
        let ui = &app[app.find("fn ui(&mut self").expect("ui")..];
        let ui = &ui[..ui.find("\n    fn on_exit(").expect("on_exit")];
        let escape = ui.find("consume_escape(").expect("escape");
        let draw = ui.find("ui::draw(").expect("draw");
        let prompt = ui.find("self.updates.draw(").expect("prompt");
        assert!(escape < draw && draw < prompt);
        let logic = &app[app.find("fn logic(").expect("logic")..];
        let logic = &logic[..logic.find("\n    }\n").expect("end")];
        let shutdown = logic.find("take_shutdown(").expect("shutdown");
        let close = logic.find("self.handle_close(").expect("close");
        assert!(shutdown < close);
        let main = include_str!("../main.rs");
        let main_fn = &main[main.find("fn main()").expect("main")..];
        let apply = main_fn.find("apply_if_requested()").expect("apply");
        let first = main_fn.find("startup::first_try(").expect("first");
        assert!(apply < first);
    }

    #[test]
    fn a_build_without_a_stamp_does_not_start_the_worker() {
        let updates = Updates::spawn(Launch { stamp: None }, || {});
        assert!(matches!(*lock_phase(&updates.phase), Phase::Quiet));
        assert!(!updates.take_shutdown());
        updates.shutdown.store(true, Ordering::SeqCst);
        assert!(updates.take_shutdown());
        assert!(!updates.take_shutdown());
    }

    #[cfg(not(windows))]
    #[test]
    fn this_build_does_not_open_a_socket_for_the_feed() {
        assert_eq!(fetch_offer().unwrap_err(), check_text());
        assert_eq!(download_archive("VERSION").unwrap_err(), payload_text());
    }

    #[test]
    fn a_relative_redirect_stays_on_the_feed_host() {
        let next = resolve_redirect(
            "https://github.com/jaysonsantos/thinwire/releases/download/nightly/VERSION",
            "SHA256SUMS",
        )
        .expect("relative");
        assert!(redirect_allowed(&next), "{next}");
        assert!(next.ends_with("/SHA256SUMS"));
        let refused = resolve_redirect(
            "https://github.com/jaysonsantos/thinwire/releases/download/nightly/VERSION",
            "https://example.com/VERSION",
        )
        .expect("absolute");
        assert!(!redirect_allowed(&refused));
    }

    #[test]
    fn swap_replaces_the_exes_and_a_broken_slot_restores_the_ledger_error() {
        let layout = Layout::new("swap");
        let (bytes, digest) = good_archive();
        stage_verified(&bytes, &digest, &layout.journal, &layout.record()).expect("stage");
        let ledger_file = layout.install.join("ledger.json");
        assert!(matches!(
            swap_files(&layout.record(), &ledger_file, &layout.journal).expect("swap"),
            SwapEnd::Installed
        ));
        for exe in EXES {
            assert_eq!(
                std::fs::read(layout.install.join(exe)).expect("live"),
                format!("new-{exe}").into_bytes()
            );
            assert!(old_path(&layout.install, exe).is_file());
            assert!(!incoming_path(&layout.install, exe).exists());
        }
        assert_eq!(
            std::fs::read(layout.install.join("LICENSE")).expect("license"),
            b"mit\n"
        );
        assert!(!layout.journal.exists());
        let ledger = load_ledger(&ledger_file);
        assert_eq!(ledger.proven.expect("proven").rev, 277);
        assert!(ledger.swap_error.is_none());

        let broken = Layout::new("swap-broken");
        std::fs::write(incoming_path(&broken.install, EXES[0]), b"new").expect("in");
        std::fs::write(old_path(&broken.install, EXES[0]), b"older").expect("old");
        std::fs::create_dir_all(broken.journal.parent().expect("dir")).expect("dir");
        std::fs::write(&broken.journal, b"{}").expect("journal");
        let broken_ledger = broken.install.join("ledger.json");
        assert_eq!(
            swap_files(&broken.record(), &broken_ledger, &broken.journal).unwrap_err(),
            swap_text()
        );
        assert_eq!(load_ledger(&broken_ledger).swap_error, Some(swap_text()));
        assert!(!broken.journal.exists());
        assert_eq!(
            std::fs::read(broken.install.join(EXES[0])).expect("live"),
            b"old-thinwire.exe"
        );
    }

    #[test]
    fn cleanup_after_our_own_sha_drops_the_old_names_and_records_proven() {
        let layout = Layout::new("cleanup");
        std::fs::create_dir_all(&layout.staging).expect("staging");
        std::fs::write(old_path(&layout.install, EXES[0]), b"previous").expect("old");
        std::fs::write(incoming_path(&layout.install, EXES[0]), b"partial").expect("in");
        let ledger_file = layout.install.join("ledger.json");
        let mut ledger = Ledger {
            deferred_rev: Some(277),
            swap_error: Some(swap_text()),
            ..Ledger::default()
        };
        store_ledger(&ledger_file, &ledger).expect("ledger");
        cleanup_swap(&layout.record(), &layout.journal, &ledger_file);
        assert_eq!(
            std::fs::read(layout.install.join(EXES[0])).expect("live"),
            b"old-thinwire.exe"
        );
        assert!(!old_path(&layout.install, EXES[0]).exists());
        assert!(!incoming_path(&layout.install, EXES[0]).exists());
        assert!(!layout.staging.exists());
        ledger = load_ledger(&ledger_file);
        assert_eq!(ledger.proven.expect("proven").sha7.as_str(), "0c07a9c");
        assert!(ledger.deferred_rev.is_none());
        assert!(ledger.swap_error.is_none());
    }
}
