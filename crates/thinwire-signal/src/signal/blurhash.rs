// SPDX-License-Identifier: AGPL-3.0-only
//! The blurred picture of a Signal attachment (`AttachmentPointer::blur_hash`).
//!
//! A hash is 6 to 166 characters of base 83. It holds the mean color of the
//! picture and some cosine parts. The picture of a video shows from it
//! before the video downloads, as the Signal apps do.

/// The digits of base 83, in the order of their values.
const DIGITS: &[u8; 83] =
    b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz#$%*+,-.:;=?@[]^_{|}~";

/// The largest side of the picture that [`decode`] makes, in pixels.
const MAX_SIDE: u32 = 64;

/// The RGB pixels of `hash`, three bytes for each pixel of `width` x
/// `height`. `None` when the hash is not valid, or when a side is zero or
/// larger than 64 pixels.
pub(super) fn decode(hash: &str, width: u32, height: u32) -> Option<Vec<u8>> {
    if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
        return None;
    }
    let digits: Vec<u32> = hash
        .bytes()
        .map(|byte| DIGITS.iter().position(|digit| *digit == byte))
        .map(|value| value.and_then(|value| u32::try_from(value).ok()))
        .collect::<Option<_>>()?;
    let size = *digits.first()?;
    let (parts_x, parts_y) = ((size % 9 + 1) as usize, (size / 9 + 1) as usize);
    if digits.len() != 4 + 2 * parts_x * parts_y {
        return None;
    }
    let number = |range: std::ops::Range<usize>| {
        digits[range]
            .iter()
            .fold(0_u32, |value, digit| value * 83 + digit)
    };
    let scale = (f64::from(digits[1]) + 1.0) / 166.0;
    let mean = number(2..6);
    let mut colors = vec![[
        linear(mean >> 16 & 255),
        linear(mean >> 8 & 255),
        linear(mean & 255),
    ]];
    for part in 1..parts_x * parts_y {
        let value = number(4 + part * 2..6 + part * 2);
        colors.push([
            signed_square(value / (19 * 19)) * scale,
            signed_square(value / 19 % 19) * scale,
            signed_square(value % 19) * scale,
        ]);
    }
    let mut pixels = Vec::with_capacity((width * height * 3) as usize);
    for y in 0..height {
        for x in 0..width {
            let mut pixel = [0.0_f64; 3];
            for (index, color) in colors.iter().enumerate() {
                // The part counts are 9 at most, so the casts are exact.
                let (i, j) = ((index % parts_x) as f64, (index / parts_x) as f64);
                let basis = (std::f64::consts::PI * f64::from(x) * i / f64::from(width)).cos()
                    * (std::f64::consts::PI * f64::from(y) * j / f64::from(height)).cos();
                for (sum, value) in pixel.iter_mut().zip(color) {
                    *sum += value * basis;
                }
            }
            pixels.extend(pixel.map(srgb));
        }
    }
    Some(pixels)
}

/// One sRGB value from 0 to 255 as linear light.
fn linear(value: u32) -> f64 {
    let value = f64::from(value) / 255.0;
    if value <= 0.040_45 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

/// Linear light as one sRGB value from 0 to 255.
fn srgb(value: f64) -> u8 {
    let value = value.clamp(0.0, 1.0);
    let value = if value <= 0.003_130_8 {
        value * 12.92
    } else {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    };
    // The value is from 0 to 255 after the clamp.
    (value * 255.0 + 0.5).clamp(0.0, 255.0) as u8
}

/// One cosine part from its digit of 0 to 18: the sign stays, the size is
/// squared.
fn signed_square(digit: u32) -> f64 {
    let value = (f64::from(digit) - 9.0) / 9.0;
    value * value.abs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base83(mut value: u32, length: usize) -> String {
        let mut digits = vec![b'0'; length];
        for slot in digits.iter_mut().rev() {
            *slot = DIGITS[(value % 83) as usize];
            value /= 83;
        }
        String::from_utf8(digits).expect("ascii")
    }

    /// A hash with only its mean color gives that color for every pixel.
    #[test]
    fn a_hash_with_one_part_is_one_color() {
        let hash = format!("00{}", base83(200 << 16 | 40 << 8 | 90, 4));
        let pixels = decode(&hash, 4, 2).expect("pixels");
        assert_eq!(pixels.len(), 4 * 2 * 3);
        for pixel in pixels.chunks(3) {
            assert_eq!(pixel, [200, 40, 90]);
        }
    }

    /// The sample hash of the format has 4 x 3 parts. Its picture is not
    /// one color: the cosine parts count.
    #[test]
    fn the_sample_hash_decodes_to_a_picture() {
        let pixels = decode("LEHV6nWB2yk8pyo0adR*.7kCMdnj", 32, 32).expect("pixels");
        assert_eq!(pixels.len(), 32 * 32 * 3);
        assert_ne!(pixels[0..3], pixels[pixels.len() - 3..]);
    }

    /// A cosine part with the middle digit adds nothing.
    #[test]
    fn a_zero_part_changes_no_pixel() {
        let zero = 9 * 19 * 19 + 9 * 19 + 9;
        let hash = format!("10{}{}", base83(0x80_80_80, 4), base83(zero, 2));
        let pixels = decode(&hash, 8, 8).expect("pixels");
        assert!(pixels.iter().all(|value| *value == 0x80));
    }

    #[test]
    fn a_bad_hash_or_size_gives_nothing() {
        assert!(decode("", 8, 8).is_none());
        assert!(
            decode("LEHV6nWB2yk8pyo0adR*.7kCMdn", 8, 8).is_none(),
            "short"
        );
        assert!(
            decode("LEHV6nWB2yk8pyo0adR*.7kCMdn!", 8, 8).is_none(),
            "digit"
        );
        assert!(decode("LEHV6nWB2yk8pyo0adR*.7kCMdnj", 0, 8).is_none());
        assert!(decode("LEHV6nWB2yk8pyo0adR*.7kCMdnj", 8, 65).is_none());
        assert!(decode("é0000000", 8, 8).is_none());
    }
}
