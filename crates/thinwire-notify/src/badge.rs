//! The picture of a notification: the thinwire mark with the tile of the
//! protocol in its corner.
//!
//! The five PNG files are in `assets/`. A snapshot test of the app
//! (`ui_snapshots::notify_badge`) makes them from the painters of the
//! sidebar, because this crate has no egui. Each OS takes a picture as a
//! file, so this module writes the bytes to a private folder. No path goes
//! to a log.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use thinwire_core::ProtocolId;
use thinwire_protocol::ProtocolKind;

/// The PNG of a protocol, and the name of its file.
const fn picture(kind: ProtocolKind) -> (&'static str, &'static [u8]) {
    match kind {
        ProtocolKind::Telegram => ("telegram", include_bytes!("../assets/badge-telegram.png")),
        ProtocolKind::WhatsApp => ("whatsapp", include_bytes!("../assets/badge-whatsapp.png")),
        ProtocolKind::Discord => ("discord", include_bytes!("../assets/badge-discord.png")),
        ProtocolKind::Slack => ("slack", include_bytes!("../assets/badge-slack.png")),
        ProtocolKind::Signal => ("signal", include_bytes!("../assets/badge-signal.png")),
    }
}

/// Write `bytes` to a new file `name` in `dir`, for this user only. The
/// folder is made when it is missing. A file that exists is an error.
fn write_new(dir: &Path, name: &str, bytes: &[u8]) -> Option<PathBuf> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .ok()?;
    let file = dir.join(name);
    let mut out = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&file)
        .ok()?;
    if out.write_all(bytes).is_err() {
        drop(out);
        let _ = fs::remove_file(&file);
        return None;
    }
    Some(file)
}

/// A new file with the picture of `protocol` in `dir`. Each call makes one
/// more file: macOS moves the file of a notification into its own store.
#[cfg(any(target_os = "macos", test))]
fn fresh_in(dir: &Path, protocol: ProtocolId) -> Option<PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let (name, bytes) = picture(protocol.kind());
    let number = NEXT.fetch_add(1, Ordering::Relaxed);
    let name = format!("badge-{name}-{}-{number}.png", std::process::id());
    write_new(dir, &name, bytes)
}

/// macOS: a new picture file for one notification, in the temporary folder
/// of the user. `None` shows the notification with no picture.
#[cfg(target_os = "macos")]
pub(crate) fn fresh_file(protocol: ProtocolId) -> Option<PathBuf> {
    fresh_in(&std::env::temp_dir().join("thinwire-notify"), protocol)
}

/// The one file with the picture of `protocol` in `dir`. A file of an
/// earlier run is written again, so a new build shows its own picture.
#[cfg(any(target_os = "linux", test))]
fn shared_in(dir: &Path, protocol: ProtocolId) -> Option<PathBuf> {
    let (name, bytes) = picture(protocol.kind());
    let file = dir.join(format!("badge-{name}.png"));
    // Write beside it, then rename: the notification server never reads
    // half a file.
    let part = write_new(
        dir,
        &format!("badge-{name}-{}.part", std::process::id()),
        bytes,
    )?;
    if fs::rename(&part, &file).is_err() {
        let _ = fs::remove_file(&part);
        return None;
    }
    Some(file)
}

/// Linux: the picture file of `protocol` in the runtime folder of the user
/// (`XDG_RUNTIME_DIR`, which only that user reads). It is written one time
/// for each protocol in a run. `None` shows the notification with no
/// picture: a system with no runtime folder gets none, because `/tmp` is
/// open to every user.
#[cfg(target_os = "linux")]
pub(crate) fn shared_file(protocol: ProtocolId) -> Option<PathBuf> {
    use std::collections::HashMap;
    use std::sync::{Mutex, PoisonError};
    static WRITTEN: Mutex<Option<HashMap<ProtocolKind, Option<PathBuf>>>> = Mutex::new(None);
    let mut written = WRITTEN.lock().unwrap_or_else(PoisonError::into_inner);
    written
        .get_or_insert_with(HashMap::new)
        .entry(protocol.kind())
        .or_insert_with(|| {
            let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?);
            if !runtime.is_absolute() {
                return None;
            }
            shared_in(&runtime.join("thinwire").join("notify"), protocol)
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";

    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "thinwire-notify-test-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn every_protocol_has_a_png_picture_of_its_own() {
        let mut seen = Vec::new();
        for kind in ProtocolKind::ALL {
            let (name, bytes) = picture(kind);
            assert!(bytes.starts_with(PNG_MAGIC), "{name}");
            assert!(!seen.contains(&bytes), "{name} repeats a picture");
            seen.push(bytes);
        }
    }

    #[test]
    fn each_fresh_file_is_new_and_private() {
        let dir = folder("fresh");
        let first = fresh_in(&dir, ProtocolId::Telegram).expect("first file");
        let second = fresh_in(&dir, ProtocolId::Telegram).expect("second file");
        assert_ne!(first, second);
        assert_eq!(
            fs::read(&first).expect("read"),
            picture(ProtocolKind::Telegram).1
        );
        let mode = |path: &Path| fs::metadata(path).expect("metadata").permissions().mode() & 0o777;
        assert_eq!(mode(&first), 0o600);
        assert_eq!(mode(&dir), 0o700);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_shared_file_is_written_again_over_an_old_one() {
        let dir = folder("shared");
        let file = shared_in(&dir, ProtocolId::Signal).expect("file");
        fs::write(&file, b"old").expect("old picture");
        assert_eq!(shared_in(&dir, ProtocolId::Signal).as_ref(), Some(&file));
        assert_eq!(
            fs::read(&file).expect("read"),
            picture(ProtocolKind::Signal).1
        );
        // An account after account 0 has the picture of its protocol.
        let second = ProtocolId::account(ProtocolKind::Signal, 1).expect("account 1");
        assert_eq!(shared_in(&dir, second), Some(file));
        let _ = fs::remove_dir_all(&dir);
    }
}
