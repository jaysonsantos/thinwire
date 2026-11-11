//! The picture of a notification: the picture of the chat with the tile
//! of the protocol in its bottom right corner.
//!
//! A chat with no picture gets the thinwire mark with the same tile. The
//! PNG files are in `assets/`: `tile-*` is the tile alone and `badge-*` is
//! the mark with the tile. A snapshot test of the app
//! (`ui_snapshots::notify_badge`) makes them from the painters of the
//! sidebar, because this crate has no egui. Each OS takes a picture as a
//! file, so this module writes the bytes to a private folder. No path goes
//! to a log.

use std::fs;
use std::io::{Cursor, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use thinwire_core::notify::Notification;
use thinwire_protocol::{ChatAvatar, ProtocolKind};

/// The mark with the tile, the tile alone, and the name of the protocol.
const fn assets(kind: ProtocolKind) -> (&'static str, &'static [u8], &'static [u8]) {
    macro_rules! of {
        ($name:literal) => {
            (
                $name,
                include_bytes!(concat!("../assets/badge-", $name, ".png")),
                include_bytes!(concat!("../assets/tile-", $name, ".png")),
            )
        };
    }
    match kind {
        ProtocolKind::Telegram => of!("telegram"),
        ProtocolKind::WhatsApp => of!("whatsapp"),
        ProtocolKind::Discord => of!("discord"),
        ProtocolKind::Slack => of!("slack"),
        ProtocolKind::Signal => of!("signal"),
    }
}

/// Side of the picture, in pixels.
const SIDE: u32 = 256;
/// Side of the protocol tile on the picture.
const TILE: u32 = 96;
/// Corner radius of the tile, as in the sidebar (0.28 of its side).
const TILE_CORNER: f32 = 27.0;
/// Width of the clear ring between the chat picture and the tile.
const RING: f32 = 7.0;
/// The largest chat picture file this module reads.
const AVATAR_MAX_BYTES: u64 = 8 * 1024 * 1024;
/// The longest side of a chat picture that it decodes.
const AVATAR_MAX_SIDE: u32 = 4096;

/// How much of the pixel at `(x, y)` is in a rounded square: 0.0 to 1.0.
/// `center` and `half` give the square, and `corner` is its radius. A
/// `corner` equal to `half` gives a circle.
fn cover(x: u32, y: u32, center: (f32, f32), half: f32, corner: f32) -> f32 {
    let dx = ((x as f32 + 0.5 - center.0).abs() - (half - corner)).max(0.0);
    let dy = ((y as f32 + 0.5 - center.1).abs() - (half - corner)).max(0.0);
    (0.5 - (dx.hypot(dy) - corner)).clamp(0.0, 1.0)
}

/// The encoded picture of the chat. `None` when the chat has none, or the
/// file cannot be read or is too large.
fn avatar_bytes(avatar: &ChatAvatar) -> Option<Vec<u8>> {
    if let Some(bytes) = avatar.bytes.as_ref().filter(|bytes| !bytes.is_empty()) {
        return Some(bytes.as_slice().to_vec());
    }
    let file = fs::File::open(avatar.path.as_ref()?.as_str()).ok()?;
    let mut bytes = Vec::new();
    file.take(AVATAR_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() as u64 <= AVATAR_MAX_BYTES).then_some(bytes)
}

fn decode(bytes: &[u8]) -> Option<DynamicImage> {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(AVATAR_MAX_SIDE);
    limits.max_image_height = Some(AVATAR_MAX_SIDE);
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    reader.limits(limits);
    reader.decode().ok()
}

/// The chat picture as a circle, with the tile of the protocol in the
/// bottom right corner and a clear ring around the tile.
fn composed(avatar: &[u8], tile: &[u8]) -> Option<Vec<u8>> {
    let face = decode(avatar)?
        .resize_to_fill(SIDE, SIDE, image::imageops::FilterType::Triangle)
        .to_rgba8();
    let tile = decode(tile)?
        .resize_exact(TILE, TILE, image::imageops::FilterType::Triangle)
        .to_rgba8();
    let middle = SIDE as f32 / 2.0;
    let tile_half = TILE as f32 / 2.0;
    let tile_middle = SIDE as f32 - tile_half;
    let origin = SIDE - TILE;
    let out = RgbaImage::from_fn(SIDE, SIDE, |x, y| {
        let Rgba([r, g, b, a]) = *face.get_pixel(x, y);
        let in_circle = cover(x, y, (middle, middle), middle, middle);
        let in_ring = cover(
            x,
            y,
            (tile_middle, tile_middle),
            tile_half + RING,
            TILE_CORNER + RING,
        );
        let in_tile = cover(x, y, (tile_middle, tile_middle), tile_half, TILE_CORNER);
        let under = f32::from(a) / 255.0 * in_circle * (1.0 - in_ring);
        let top = if x >= origin && y >= origin {
            *tile.get_pixel(x - origin, y - origin)
        } else {
            Rgba([0, 0, 0, 0])
        };
        // The tile over the chat picture. Where the tile is, the ring has
        // made the chat picture clear, so no mix of colors is necessary.
        if in_tile > 0.0 {
            Rgba([top[0], top[1], top[2], (in_tile * 255.0).round() as u8])
        } else {
            Rgba([r, g, b, (under * 255.0).round() as u8])
        }
    });
    let mut png = Vec::new();
    out.write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
        .ok()?;
    Some(png)
}

/// The PNG picture of `notification`, and the name of its protocol.
fn picture(notification: &Notification) -> (&'static str, Vec<u8>) {
    let (name, mark, tile) = assets(notification.key.protocol.kind());
    let png = avatar_bytes(&notification.avatar)
        .and_then(|avatar| composed(&avatar, tile))
        .unwrap_or_else(|| mark.to_vec());
    (name, png)
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

/// A number that is new for each call in this run.
fn next_number() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// A new file with the picture of `notification` in `dir`. Each call makes
/// one more file: macOS moves the file of a notification into its own
/// store.
#[cfg(any(target_os = "macos", test))]
fn fresh_in(dir: &Path, notification: &Notification) -> Option<PathBuf> {
    let (name, png) = picture(notification);
    let name = format!("badge-{name}-{}-{}.png", std::process::id(), next_number());
    write_new(dir, &name, &png)
}

/// macOS: a new picture file for one notification, in the temporary folder
/// of the user. `None` shows the notification with no picture.
#[cfg(target_os = "macos")]
pub(crate) fn fresh_file(notification: &Notification) -> Option<PathBuf> {
    fresh_in(&std::env::temp_dir().join("thinwire-notify"), notification)
}

/// The files that Linux keeps: a notification takes the next one, and
/// after the last it starts again at the first.
#[cfg(any(target_os = "linux", test))]
const SLOTS: u64 = 32;

/// The picture of `notification` in the next of the `SLOTS` files of
/// `dir`. A notification server reads the file when it shows the
/// notification, so a file stays until `SLOTS` more notifications came.
#[cfg(any(target_os = "linux", test))]
fn slot_in(dir: &Path, notification: &Notification) -> Option<PathBuf> {
    let (_, png) = picture(notification);
    let number = next_number();
    let file = dir.join(format!("picture-{}.png", number % SLOTS));
    // Write beside it, then rename: the notification server never reads
    // half a file.
    let part = write_new(
        dir,
        &format!("picture-{}-{number}.part", std::process::id()),
        &png,
    )?;
    if fs::rename(&part, &file).is_err() {
        let _ = fs::remove_file(&part);
        return None;
    }
    Some(file)
}

/// Linux: the picture file of `notification` in the runtime folder of the
/// user (`XDG_RUNTIME_DIR`, which only that user reads). `None` shows the
/// notification with no picture: a system with no runtime folder gets
/// none, because `/tmp` is open to every user.
#[cfg(target_os = "linux")]
pub(crate) fn slot_file(notification: &Notification) -> Option<PathBuf> {
    let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?);
    if !runtime.is_absolute() {
        return None;
    }
    slot_in(&runtime.join("thinwire").join("notify"), notification)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use thinwire_core::ProtocolId;
    use thinwire_core::notify::NotifyKey;
    use thinwire_protocol::{ImageBytes, ImagePath};

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

    fn note(protocol: ProtocolId, avatar: ChatAvatar) -> Notification {
        Notification {
            key: NotifyKey {
                protocol,
                conversation_id: "chat".into(),
                topic: None,
            },
            title: "Ada".into(),
            sender: None,
            preview: "hi".into(),
            count: 1,
            avatar,
        }
    }

    /// A red square, as the encoded picture of a chat.
    fn red_png() -> Vec<u8> {
        let mut png = Vec::new();
        RgbaImage::from_pixel(40, 40, Rgba([255, 0, 0, 255]))
            .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
            .expect("encode");
        png
    }

    fn with_bytes(bytes: Vec<u8>) -> ChatAvatar {
        ChatAvatar {
            bytes: Some(ImageBytes::new(bytes)),
            ..Default::default()
        }
    }

    #[test]
    fn every_protocol_has_pictures_of_its_own() {
        let mut seen = Vec::new();
        for kind in ProtocolKind::ALL {
            let (name, mark, tile) = assets(kind);
            for bytes in [mark, tile] {
                assert!(bytes.starts_with(PNG_MAGIC), "{name}");
                assert!(!seen.contains(&bytes), "{name} repeats a picture");
                seen.push(bytes);
            }
        }
    }

    #[test]
    fn a_chat_picture_is_a_circle_with_the_tile_in_its_corner() {
        let (_, png) = picture(&note(ProtocolId::Telegram, with_bytes(red_png())));
        let image = decode(&png).expect("png").to_rgba8();
        assert_eq!(image.dimensions(), (SIDE, SIDE));
        // The middle is the chat picture, and the top left corner is
        // outside the circle.
        assert_eq!(*image.get_pixel(100, 100), Rgba([255, 0, 0, 255]));
        assert_eq!(image.get_pixel(2, 2)[3], 0);
        // The middle of the tile is not the chat picture.
        let tile = *image.get_pixel(SIDE - TILE / 2, SIDE - TILE / 8);
        assert_eq!(tile[3], 255);
        assert_ne!(tile, Rgba([255, 0, 0, 255]));
        // The ring between them is clear.
        assert_eq!(image.get_pixel(SIDE - TILE / 2, SIDE - TILE - 4)[3], 0);
    }

    #[test]
    fn a_chat_picture_file_is_read() {
        let dir = folder("avatar");
        fs::create_dir_all(&dir).expect("folder");
        let file = dir.join("a.png");
        fs::write(&file, red_png()).expect("picture");
        let avatar = ChatAvatar {
            path: Some(ImagePath::new(file.to_string_lossy())),
            ..Default::default()
        };
        let (_, png) = picture(&note(ProtocolId::Signal, avatar));
        let image = decode(&png).expect("png").to_rgba8();
        assert_eq!(*image.get_pixel(100, 100), Rgba([255, 0, 0, 255]));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_chat_picture_or_a_bad_one_gives_the_mark_of_the_protocol() {
        for kind in ProtocolKind::ALL {
            let protocol = ProtocolId::primary(kind);
            let mark = assets(kind).1;
            assert_eq!(picture(&note(protocol, ChatAvatar::default())).1, mark);
            let bad = with_bytes(b"not an image".to_vec());
            assert_eq!(picture(&note(protocol, bad)).1, mark);
        }
        // An account after account 0 has the picture of its protocol.
        let second = ProtocolId::account(ProtocolKind::Signal, 1).expect("account 1");
        assert_eq!(
            picture(&note(second, ChatAvatar::default())).1,
            assets(ProtocolKind::Signal).1
        );
    }

    #[test]
    fn each_fresh_file_is_new_and_private() {
        let dir = folder("fresh");
        let note = note(ProtocolId::Telegram, ChatAvatar::default());
        let first = fresh_in(&dir, &note).expect("first file");
        let second = fresh_in(&dir, &note).expect("second file");
        assert_ne!(first, second);
        assert_eq!(
            fs::read(&first).expect("read"),
            assets(ProtocolKind::Telegram).1
        );
        let mode = |path: &Path| fs::metadata(path).expect("metadata").permissions().mode() & 0o777;
        assert_eq!(mode(&first), 0o600);
        assert_eq!(mode(&dir), 0o700);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_slot_file_is_written_again_over_an_old_one() {
        let dir = folder("slots");
        let note = note(ProtocolId::Signal, ChatAvatar::default());
        let mut files = std::collections::HashSet::new();
        for _ in 0..=SLOTS * 2 {
            let file = slot_in(&dir, &note).expect("file");
            assert_eq!(
                fs::read(&file).expect("read"),
                assets(ProtocolKind::Signal).1
            );
            files.insert(file);
        }
        assert!(files.len() as u64 <= SLOTS);
        assert_eq!(fs::read_dir(&dir).expect("folder").count(), files.len());
        let _ = fs::remove_dir_all(&dir);
    }
}
