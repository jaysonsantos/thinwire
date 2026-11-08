use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use base64::Engine;
use ed25519_dalek::VerifyingKey;

use super::machine::{
    self, ArchiveFile, BuildId, Feed, MAX_APPCAST, MAX_DOWNLOAD, MAX_RELEASE_JSON, Offer,
    developer_id_text, follow_redirect,
};
use super::{BAD_SIGNATURE, CHECK_FAILED, NOT_WRITABLE, PlainError, SWAP_FAILED};

const NIGHTLY_ARCH: &str = "aarch64";
/// The nightly release, read through the REST API. The download URL of a
/// release asset is cached for many minutes, and `appcast.xml` and
/// `appcast.ed25519` keep their names on every nightly. Through the download
/// URL, a new feed can come with an old signature. The API is never stale.
const RELEASE_API_URL: &str =
    "https://api.github.com/repos/jaysonsantos/thinwire/releases/tags/nightly";
const GITHUB_JSON: &str = "application/vnd.github+json";
const ASSET_BYTES: &str = "application/octet-stream";
/// The GitHub API refuses a request without a User-Agent.
const USER_AGENT: &str = concat!("thinwire/", env!("CARGO_PKG_VERSION"));
const FEED_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) enum Gate {
    NotClient,
    Translocated,
    Client { our: BuildId, dest: PathBuf },
}

pub(super) fn gate() -> Gate {
    if std::env::consts::ARCH != NIGHTLY_ARCH {
        return Gate::NotClient;
    }
    let Ok(exe) = std::env::current_exe() else {
        return Gate::NotClient;
    };
    let Some(bundle) = bundle_from_exe(&exe) else {
        return Gate::NotClient;
    };
    let Some(our) = read_build_id(&bundle) else {
        return Gate::NotClient;
    };
    let signed = codesign_verify(&bundle)
        && codesign_text(&bundle).is_some_and(|text| developer_id_text(&text));
    let moved = translocated(&exe) || translocated(&bundle);
    match (signed, moved) {
        (true, true) => Gate::Translocated,
        (true, false) => Gate::Client { our, dest: bundle },
        (false, _) => Gate::NotClient,
    }
}

pub(super) fn fetch_feed(_our: &BuildId) -> Result<Feed, PlainError> {
    let assets = match https_bytes(
        RELEASE_API_URL,
        GITHUB_JSON,
        MAX_RELEASE_JSON as u64,
        FEED_TIMEOUT,
    )
    .and_then(|json| machine::feed_assets(&json))
    {
        Ok(assets) => assets,
        Err(error) => {
            log_kind("check_failed");
            return Err(error);
        }
    };
    let appcast = match https_bytes(
        &assets.appcast,
        ASSET_BYTES,
        MAX_APPCAST as u64,
        FEED_TIMEOUT,
    ) {
        Ok(bytes) => bytes,
        Err(error) => {
            log_kind("check_failed");
            return Err(error);
        }
    };
    let signature_text = match https_bytes(&assets.signature, ASSET_BYTES, 1024, FEED_TIMEOUT) {
        Ok(bytes) => bytes,
        Err(error) => {
            log_kind("check_failed");
            return Err(error);
        }
    };
    let Ok(signature_text) = String::from_utf8(signature_text) else {
        log_kind("bad_signature");
        return Err(BAD_SIGNATURE);
    };
    let Some(signature) = machine::decode_signature(&signature_text) else {
        log_kind("bad_signature");
        return Err(BAD_SIGNATURE);
    };
    let key = match bundled_key() {
        Ok(key) => key,
        Err(error) => {
            log_kind("bad_signature");
            return Err(error);
        }
    };
    match machine::open_feed(&appcast, &signature, &key) {
        Ok(feed) => Ok(feed),
        Err(error) if error == BAD_SIGNATURE => {
            log_kind("bad_signature");
            Err(error)
        }
        Err(error) => {
            log_kind("check_failed");
            Err(error)
        }
    }
}

pub(super) fn download(offer: &Offer) -> Result<ArchiveFile, PlainError> {
    let root = dirs::data_dir().ok_or(CHECK_FAILED)?;
    let dir = root
        .join("thinwire")
        .join("update-staging")
        .join(offer.build().display());
    fs::create_dir_all(&dir).map_err(|_| CHECK_FAILED)?;
    let partial = dir.join("archive.tar.gz.partial");
    let final_path = dir.join("archive.tar.gz");
    let declared = offer.enclosure().length();
    match stream_to(offer.enclosure().url(), declared, &partial) {
        Ok(written) if written == declared => {}
        Ok(_) | Err(_) => {
            let _ = fs::remove_file(&partial);
            log_kind("check_failed");
            return Err(CHECK_FAILED);
        }
    }
    if verify_file(&partial, offer.enclosure().signature()).is_err() {
        let _ = fs::remove_file(&partial);
        log_kind("bad_signature");
        return Err(BAD_SIGNATURE);
    }
    fs::rename(&partial, &final_path).map_err(|_| CHECK_FAILED)?;
    Ok(ArchiveFile::new(final_path))
}

pub(super) fn verify_and_arm(archive: &ArchiveFile, dest: &Path) -> Result<(), PlainError> {
    let Some(parent) = archive.path().parent() else {
        log_kind("bad_signature");
        return Err(BAD_SIGNATURE);
    };
    let bundle = match machine::extract_archive(archive.path(), parent) {
        Ok(bundle) => bundle,
        Err(error) => {
            log_kind(if error == BAD_SIGNATURE {
                "bad_signature"
            } else {
                "check_failed"
            });
            return Err(error);
        }
    };
    if !helpers_are_files(&bundle) {
        log_kind("bad_signature");
        return Err(BAD_SIGNATURE);
    }
    let bundle = match verified(bundle) {
        Ok(bundle) => bundle,
        Err(error) => return Err(error),
    };
    if !probe_writable(dest) {
        log_kind("swap_failed");
        return Err(NOT_WRITABLE);
    }
    spawn_apply(bundle, dest)
}

pub(super) fn require_developer_id(bundle: &Path) -> Result<(), PlainError> {
    if codesign_verify(bundle) && codesign_text(bundle).is_some_and(|text| developer_id_text(&text))
    {
        Ok(())
    } else {
        log_kind("codesign_rejected");
        Err(BAD_SIGNATURE)
    }
}

struct VerifiedBundle {
    path: PathBuf,
}

fn verified(path: PathBuf) -> Result<VerifiedBundle, PlainError> {
    require_developer_id(&path)?;
    Ok(VerifiedBundle { path })
}

fn helpers_are_files(bundle: &Path) -> bool {
    for name in ["thinwire-whatsapp-helper", "thinwire-signal-helper"] {
        let path = bundle.join("Contents/MacOS").join(name);
        let Ok(meta) = fs::symlink_metadata(&path) else {
            return false;
        };
        if !meta.is_file() {
            return false;
        }
    }
    true
}

fn probe_writable(dest: &Path) -> bool {
    let Some(parent) = dest.parent() else {
        return false;
    };
    let probe = parent.join(".thinwire-update-probe");
    let created = File::create(&probe);
    let _ = fs::remove_file(&probe);
    created.is_ok()
}

fn spawn_apply(bundle: VerifiedBundle, dest: &Path) -> Result<(), PlainError> {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;

    let exe = std::env::current_exe().map_err(|_| SWAP_FAILED)?;
    let mut command = Command::new(exe);
    command
        .arg("--apply-update")
        .arg(&bundle.path)
        .arg(dest)
        .stdin(Stdio::piped())
        .process_group(0);
    match command.spawn() {
        Ok(child) => {
            // Drop during shutdown would close the pipe while adapters are still stopping.
            std::mem::forget(child);
            Ok(())
        }
        Err(_) => {
            log_kind("swap_failed");
            Err(SWAP_FAILED)
        }
    }
}

fn bundled_key() -> Result<VerifyingKey, PlainError> {
    let text = include_str!("../../update-public-key.txt");
    let bytes = raw_public_key(text).ok_or(BAD_SIGNATURE)?;
    VerifyingKey::from_bytes(&bytes).map_err(|_| BAD_SIGNATURE)
}

fn raw_public_key(text: &str) -> Option<[u8; 32]> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(text.trim())
        .ok()?;
    bytes.try_into().ok()
}

/// The build id of the running app, for Settings. `None` outside a bundle.
pub(super) fn display_build() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let bundle = bundle_from_exe(&exe)?;
    read_build_id(&bundle).map(|build| build.display())
}

fn read_build_id(bundle: &Path) -> Option<BuildId> {
    let text = fs::read_to_string(bundle.join("Contents/Resources/BUILD_ID")).ok()?;
    BuildId::parse(&text).ok()
}

fn bundle_from_exe(exe: &Path) -> Option<PathBuf> {
    let exe = exe.canonicalize().unwrap_or_else(|_| exe.to_path_buf());
    let macos = exe.parent()?;
    if exe.file_name() != Some(std::ffi::OsStr::new("thinwire")) {
        return None;
    }
    if macos.file_name() != Some(std::ffi::OsStr::new("MacOS")) {
        return None;
    }
    let contents = macos.parent()?;
    if contents.file_name() != Some(std::ffi::OsStr::new("Contents")) {
        return None;
    }
    let bundle = contents.parent()?;
    if bundle.file_name() != Some(std::ffi::OsStr::new("Thinwire.app")) {
        return None;
    }
    Some(bundle.to_path_buf())
}

fn translocated(path: &Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_string_lossy()
            .contains("AppTranslocation")
    })
}

fn codesign_verify(bundle: &Path) -> bool {
    Command::new("/usr/bin/codesign")
        .args(["--verify", "--strict", "--deep"])
        .arg(bundle)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn codesign_text(bundle: &Path) -> Option<String> {
    let output = Command::new("/usr/bin/codesign")
        .args(["-dvvv"])
        .arg(bundle)
        .output()
        .ok()?;
    let mut text = String::from_utf8_lossy(&output.stderr).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stdout));
    Some(text)
}

fn agent(total: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .https_only(true)
        .redirects(0)
        .try_proxy_from_env(false)
        .timeout_connect(Duration::from_secs(20))
        .timeout(total)
        .build()
}

fn https_bytes(
    start: &str,
    accept: &str,
    cap: u64,
    total: Duration,
) -> Result<Vec<u8>, PlainError> {
    let response = fetch(start, accept, total)?;
    if let Some(header) = response.header("content-length") {
        let len: u64 = header.trim().parse().map_err(|_| CHECK_FAILED)?;
        if len > cap {
            return Err(CHECK_FAILED);
        }
    }
    let mut body = Vec::new();
    response
        .into_reader()
        .take(cap + 1)
        .read_to_end(&mut body)
        .map_err(|_| CHECK_FAILED)?;
    if body.len() as u64 > cap {
        return Err(CHECK_FAILED);
    }
    Ok(body)
}

fn stream_to(start: &str, declared: u64, dest: &Path) -> Result<u64, PlainError> {
    if declared == 0 || declared > MAX_DOWNLOAD {
        return Err(CHECK_FAILED);
    }
    let response = fetch(start, ASSET_BYTES, Duration::from_secs(15 * 60))?;
    if let Some(header) = response.header("content-length") {
        let len: u64 = header.trim().parse().map_err(|_| CHECK_FAILED)?;
        if len != declared {
            return Err(CHECK_FAILED);
        }
    }
    let mut file = File::create(dest).map_err(|_| CHECK_FAILED)?;
    let mut reader = response.into_reader();
    let mut buf = [0u8; 64 * 1024];
    let mut written = 0u64;
    loop {
        let n = reader.read(&mut buf).map_err(|_| CHECK_FAILED)?;
        if n == 0 {
            break;
        }
        written = written.saturating_add(n as u64);
        if written > declared {
            return Err(CHECK_FAILED);
        }
        file.write_all(&buf[..n]).map_err(|_| CHECK_FAILED)?;
    }
    file.sync_all().map_err(|_| CHECK_FAILED)?;
    Ok(written)
}

fn fetch(start: &str, accept: &str, total: Duration) -> Result<ureq::Response, PlainError> {
    if !start.starts_with("https://") {
        return Err(CHECK_FAILED);
    }
    let agent = agent(total);
    let mut current = start.to_string();
    let mut hops = 0u32;
    loop {
        let response = agent
            .get(&current)
            .set("accept", accept)
            .set("user-agent", USER_AGENT)
            .call()
            .map_err(|_| CHECK_FAILED)?;
        let status = response.status();
        if (300..400).contains(&status) {
            let location = response.header("location").ok_or(CHECK_FAILED)?.to_string();
            current = follow_redirect(&current, &location, hops)?;
            hops += 1;
            continue;
        }
        if status != 200 {
            return Err(CHECK_FAILED);
        }
        return Ok(response);
    }
}

fn verify_file(path: &Path, signature: &[u8; 64]) -> Result<(), PlainError> {
    let bytes = fs::read(path).map_err(|_| CHECK_FAILED)?;
    let key = bundled_key()?;
    machine::verify_detached(&bytes, signature, &key)
}

fn log_kind(kind: &'static str) {
    tracing::warn!(target: "thinwire::update", kind);
}
