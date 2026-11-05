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
