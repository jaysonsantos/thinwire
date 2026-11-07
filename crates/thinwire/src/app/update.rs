//! A Windows build compiled on `main` of this repository can replace itself
//! from the `nightly` pre-release.
//!
//! The worker thread is the only writer of [`Phase`]. The window sends
//! commands. [`Ledger`] and [`Journal`] are the only records on disk. The
//! progress of each executable is not stored: [`observe`] reads the install
//! directory.

use std::path::{Path, PathBuf};

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

const FEED_PREFIX: &str = "https://github.com/jaysonsantos/thinwire/releases/download/nightly/";

/// The feed hosts a redirect may land on. The `Location` target is not logged.
const FEED_HOSTS: [&str; 2] = ["github.com", "release-assets.githubusercontent.com"];

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

fn feed_url(name: &str) -> String {
    format!("{FEED_PREFIX}{name}")
}

/// The line whose name is the Windows archive. Zero matches or two matches
/// is an error. A filename in the file is not a URL.
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
}
