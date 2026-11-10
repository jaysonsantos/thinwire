//! A photo that fits the wire, from a downloaded image file.
//!
//! A helper sends a photo as image bytes, and one line of the pipe holds
//! 1 MiB at most. An in-app adapter uses the same limit, so a large photo
//! does not fill the memory of the app. A JPEG, PNG, or WebP file that fits
//! stays as it is. Any other image is made smaller and becomes a JPEG.

use std::io::Cursor;

use crate::media::{ImageBytes, MessagePhoto, PhotoSize};

/// The largest image that crosses the pipe. It is
/// `thinwire_ipc::WirePhoto::MAX_BYTES` (a test checks it).
pub const PHOTO_MAX_BYTES: usize = 512 * 1024;

/// The largest photo file that an adapter downloads.
pub const PHOTO_DOWNLOAD_MAX: usize = 25 * 1024 * 1024;

/// The longest side of a photo after it is made smaller, in pixels. The
/// second value is for a photo that is still too large after the first.
const PHOTO_SIDES: [u32; 2] = [1280, 800];

/// The JPEG quality for each side of [`PHOTO_SIDES`].
const PHOTO_QUALITY: [u8; 2] = [82, 70];

/// The largest image that the decoder accepts, in pixels for each side.
const DECODE_MAX_SIDE: u32 = 12_000;

/// The photo for a message, from the downloaded file. `None` when the bytes
/// are not an image, or when the image does not fit at the smallest size.
#[must_use]
pub fn photo_from_bytes(bytes: &[u8]) -> Option<MessagePhoto> {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(DECODE_MAX_SIDE);
    limits.max_image_height = Some(DECODE_MAX_SIDE);
    let reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let format = reader.format()?;
    let keep = bytes.len() <= PHOTO_MAX_BYTES
        && matches!(
            format,
            image::ImageFormat::Jpeg | image::ImageFormat::Png | image::ImageFormat::WebP
        );
    if keep {
        let mut sized = image::ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .ok()?;
        sized.limits(limits);
        let (width, height) = sized.into_dimensions().ok()?;
        return Some(photo(width, height, bytes.to_vec()));
    }
    let mut decoder = reader;
    decoder.limits(limits);
    let image = decoder.decode().ok()?;
    for (side, quality) in PHOTO_SIDES.into_iter().zip(PHOTO_QUALITY) {
        let small = if image.width().max(image.height()) > side {
            image.resize(side, side, image::imageops::FilterType::Triangle)
        } else {
            image.clone()
        };
        let rgb = small.to_rgb8();
        let mut out = Vec::new();
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
        if rgb.write_with_encoder(encoder).is_err() {
            return None;
        }
        if out.len() <= PHOTO_MAX_BYTES {
            return Some(photo(rgb.width(), rgb.height(), out));
        }
    }
    None
}

/// The longest side of a sticker after it is made smaller, in pixels.
const STICKER_SIDE: u32 = 512;

/// The image of a sticker, from the downloaded file: encoded bytes that
/// fit the pipe. A WebP or a PNG file with one picture stays as it is. An
/// animated sticker becomes a PNG of its first picture, because the app
/// draws one picture. A file that is too large for the pipe becomes a
/// smaller PNG. `None` when the bytes are not an image that the decoder
/// reads: a Lottie sticker is one.
#[must_use]
pub fn sticker_from_bytes(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(DECODE_MAX_SIDE);
    limits.max_image_height = Some(DECODE_MAX_SIDE);
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let format = reader.format()?;
    reader.limits(limits);
    let still = match format {
        image::ImageFormat::WebP => !webp_is_animated(bytes),
        image::ImageFormat::Png => !png_is_animated(bytes),
        _ => false,
    };
    if still && bytes.len() <= PHOTO_MAX_BYTES {
        // Read the header only: a file with no size is not an image.
        reader.into_dimensions().ok()?;
        return Some(bytes.to_vec());
    }
    // The decoder returns the first picture of an animation.
    let image = reader.decode().ok()?;
    let small = if image.width().max(image.height()) > STICKER_SIDE {
        image.resize(
            STICKER_SIDE,
            STICKER_SIDE,
            image::imageops::FilterType::Triangle,
        )
    } else {
        image
    };
    let mut out = Cursor::new(Vec::new());
    small
        .to_rgba8()
        .write_to(&mut out, image::ImageFormat::Png)
        .ok()?;
    let out = out.into_inner();
    (out.len() <= PHOTO_MAX_BYTES).then_some(out)
}

/// A WebP file with the animation flag in its `VP8X` header.
fn webp_is_animated(bytes: &[u8]) -> bool {
    const ANIMATION: u8 = 0x02;
    bytes.len() > 20
        && &bytes[0..4] == b"RIFF"
        && &bytes[8..12] == b"WEBP"
        && &bytes[12..16] == b"VP8X"
        && bytes[20] & ANIMATION != 0
}

/// A PNG file with an animation control chunk (`acTL`) before its image
/// data: an APNG.
fn png_is_animated(bytes: &[u8]) -> bool {
    const SIGNATURE: usize = 8;
    let mut at = SIGNATURE;
    while let Some(head) = bytes.get(at..at + 8) {
        let length = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
        match &head[4..8] {
            b"acTL" => return true,
            b"IDAT" | b"IEND" => return false,
            _ => {}
        }
        // Length, type, data, and checksum.
        let Some(next) = at.checked_add(length).and_then(|end| end.checked_add(12)) else {
            return false;
        };
        at = next;
    }
    false
}

/// A small JPEG photo from raw RGB pixels: `rgb` has three bytes for each
/// pixel of `width` x `height`. The photo shows at `shown`. `None` when the
/// sizes do not match the bytes.
#[must_use]
pub fn photo_from_rgb(
    width: u32,
    height: u32,
    rgb: Vec<u8>,
    shown: (u32, u32),
) -> Option<MessagePhoto> {
    let image = image::RgbImage::from_raw(width, height, rgb)?;
    let mut out = Vec::new();
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 80);
    image.write_with_encoder(encoder).ok()?;
    (!out.is_empty() && out.len() <= PHOTO_MAX_BYTES).then(|| photo(shown.0, shown.1, out))
}

/// The pixel size of an encoded image, from its header. `None` when the
/// bytes are not an image.
#[must_use]
pub fn image_size(bytes: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

/// The longest side of a chat picture, in pixels.
const AVATAR_SIDE: u32 = 128;

/// A small JPEG chat picture from a downloaded image. `None` when the
/// bytes are not an image.
#[must_use]
pub fn avatar_from_bytes(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(DECODE_MAX_SIDE);
    limits.max_image_height = Some(DECODE_MAX_SIDE);
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    reader.limits(limits);
    let image = reader.decode().ok()?;
    let small = image.thumbnail(AVATAR_SIDE, AVATAR_SIDE).to_rgb8();
    let mut out = Vec::new();
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 80);
    small.write_with_encoder(encoder).ok()?;
    Some(out)
}

/// A photo of one size with its bytes.
#[must_use]
pub fn photo(width: u32, height: u32, bytes: Vec<u8>) -> MessagePhoto {
    MessagePhoto {
        width,
        height,
        mini: None,
        sizes: vec![PhotoSize {
            width,
            height,
            file_id: None,
            path: None,
            bytes: Some(ImageBytes::new(bytes)),
        }],
        has_spoiler: false,
        is_secret: false,
        failed: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbImage::from_pixel(width, height, image::Rgb([200, 40, 40]));
        let mut out = Cursor::new(Vec::new());
        image
            .write_to(&mut out, image::ImageFormat::Png)
            .expect("png");
        out.into_inner()
    }

    #[test]
    fn the_wire_limit_is_the_limit_of_the_pipe() {
        assert_eq!(PHOTO_MAX_BYTES, thinwire_ipc::WirePhoto::MAX_BYTES);
    }

    #[test]
    fn a_small_photo_keeps_its_bytes() {
        let bytes = png(4, 2);
        let photo = photo_from_bytes(&bytes).expect("an image");
        assert_eq!((photo.width, photo.height), (4, 2));
        let shown = photo.sizes[0].bytes.as_ref().expect("bytes");
        assert_eq!(shown.as_slice(), bytes.as_slice());
        assert!(photo_from_bytes(b"not an image").is_none());
    }

    #[test]
    fn a_chat_picture_is_a_small_jpeg() {
        let small = avatar_from_bytes(&png(400, 200)).expect("a picture");
        assert!(small.starts_with(&[0xFF, 0xD8]));
        let image = image::load_from_memory(&small).expect("decodes");
        assert_eq!((image.width(), image.height()), (128, 64));
        assert!(avatar_from_bytes(b"not an image").is_none());
    }

    /// One lossless WebP picture of one color.
    fn webp(width: u32, height: u32, color: [u8; 4]) -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(width, height, image::Rgba(color));
        let mut out = Cursor::new(Vec::new());
        image
            .write_to(&mut out, image::ImageFormat::WebP)
            .expect("webp");
        out.into_inner()
    }

    fn chunk(name: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut out = name.to_vec();
        out.extend(u32::try_from(data.len()).expect("size").to_le_bytes());
        out.extend(data);
        if data.len() % 2 == 1 {
            out.push(0);
        }
        out
    }

    fn u24(value: u32) -> [u8; 3] {
        let bytes = value.to_le_bytes();
        [bytes[0], bytes[1], bytes[2]]
    }

    /// An animated WebP file of two pictures: red, then blue.
    fn animated_webp(width: u32, height: u32) -> Vec<u8> {
        let frame = |color: [u8; 4]| {
            let still = webp(width, height, color);
            // The picture data of a simple file: the chunk after "WEBP".
            let picture = still[12..].to_vec();
            let mut data = Vec::new();
            data.extend(u24(0));
            data.extend(u24(0));
            data.extend(u24(width - 1));
            data.extend(u24(height - 1));
            data.extend(u24(100));
            data.push(0);
            data.extend(picture);
            chunk(b"ANMF", &data)
        };
        let mut header = vec![0x02 | 0x10, 0, 0, 0];
        header.extend(u24(width - 1));
        header.extend(u24(height - 1));
        let mut body = b"WEBP".to_vec();
        body.extend(chunk(b"VP8X", &header));
        body.extend(chunk(b"ANIM", &[0, 0, 0, 0, 0, 0]));
        body.extend(frame([255, 0, 0, 255]));
        body.extend(frame([0, 0, 255, 255]));
        let mut out = b"RIFF".to_vec();
        out.extend(u32::try_from(body.len()).expect("size").to_le_bytes());
        out.extend(body);
        out
    }

    /// A still WebP or PNG sticker keeps its bytes. Bytes that are not an
    /// image, as a Lottie sticker, give no image.
    #[test]
    fn a_still_sticker_keeps_its_bytes() {
        let still = webp(8, 4, [10, 200, 30, 255]);
        assert!(!webp_is_animated(&still));
        assert_eq!(sticker_from_bytes(&still), Some(still.clone()));
        let still = png(8, 4);
        assert!(!png_is_animated(&still));
        assert_eq!(sticker_from_bytes(&still), Some(still.clone()));
        assert!(sticker_from_bytes(b"PK\x03\x04 a lottie archive").is_none());
        assert!(sticker_from_bytes(b"{\"v\":\"5.5.2\",\"layers\":[]}").is_none());
        assert!(sticker_from_bytes(b"").is_none());
    }

    /// An animated WebP sticker becomes a PNG of its first picture.
    #[test]
    fn an_animated_sticker_shows_its_first_picture() {
        let animated = animated_webp(8, 4);
        assert!(webp_is_animated(&animated));
        let shown = sticker_from_bytes(&animated).expect("the first picture");
        assert!(shown.starts_with(b"\x89PNG"), "a PNG");
        let image = image::load_from_memory(&shown).expect("decodes").to_rgba8();
        assert_eq!((image.width(), image.height()), (8, 4));
        // The decoder blends the picture on its canvas: red can be 254.
        let [red, green, blue, alpha] = image.get_pixel(3, 2).0;
        assert!(red >= 250 && green <= 5 && blue <= 5, "the red picture");
        assert_eq!(alpha, 255);
    }

    /// An APNG file has an `acTL` chunk before its image data.
    #[test]
    fn an_animated_png_is_found_by_its_control_chunk() {
        let still = png(2, 2);
        // Put an `acTL` chunk after the header chunk (8 + 25 bytes).
        let mut animated = still[..33].to_vec();
        animated.extend(8_u32.to_be_bytes());
        animated.extend(b"acTL");
        animated.extend([0, 0, 0, 1, 0, 0, 0, 0]);
        animated.extend([0, 0, 0, 0]);
        animated.extend(&still[33..]);
        assert!(png_is_animated(&animated));
        assert!(!png_is_animated(b"\x89PNG\r\n\x1a\n"));
        assert!(!png_is_animated(b""));
    }

    /// Raw pixels become a JPEG that shows at the given size.
    #[test]
    fn raw_pixels_become_a_small_jpeg() {
        let photo = photo_from_rgb(2, 2, vec![120; 12], (640, 360)).expect("a photo");
        assert_eq!((photo.width, photo.height), (640, 360));
        let bytes = photo.sizes[0].bytes.as_ref().expect("bytes");
        assert!(bytes.as_slice().starts_with(&[0xFF, 0xD8]));
        assert_eq!(image_size(bytes.as_slice()), Some((2, 2)));
        assert!(photo_from_rgb(2, 2, vec![0; 5], (1, 1)).is_none());
        assert!(image_size(b"not an image").is_none());
    }

    /// A large photo becomes a JPEG that fits the pipe, with its long side
    /// at most the first size.
    #[test]
    fn a_large_photo_is_made_smaller() {
        // Noise does not compress, so this PNG is larger than the pipe.
        let mut seed = 7_u32;
        let image = image::RgbImage::from_fn(1600, 1200, |_, _| {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let [a, b, c, _] = seed.to_le_bytes();
            image::Rgb([a, b, c])
        });
        let mut out = Cursor::new(Vec::new());
        image
            .write_to(&mut out, image::ImageFormat::Png)
            .expect("png");
        let bytes = out.into_inner();
        assert!(bytes.len() > PHOTO_MAX_BYTES);
        let photo = photo_from_bytes(&bytes).expect("a smaller photo");
        assert!(photo.width.max(photo.height) <= PHOTO_SIDES[0]);
        let shown = photo.sizes[0].bytes.as_ref().expect("bytes");
        assert!(shown.as_slice().len() <= PHOTO_MAX_BYTES);
        assert!(shown.as_slice().starts_with(&[0xFF, 0xD8]), "a JPEG");
    }
}
