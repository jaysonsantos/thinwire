//! One QR image for a Signal provisioning URL or a WhatsApp pairing payload.
//!
//! The payload may wrap on screen as several lines. It is still one string.
//! Callers pass the whole string. This module does not split lines.
//!
//! The image is built once for a pairing generation and a payload, then
//! cached as an egui texture. A later frame reuses that texture. A new
//! generation, or a new payload of the same generation, builds it again.
//! WhatsApp replaces the code while the pairing generation stays the same.

use eframe::egui::{self, TextureHandle};

/// QR modules plus a quiet zone, scaled so a phone camera can read the code.
const QR_SCALE: usize = 8;
const QR_QUIET: usize = 4;

/// On-screen side of the code, in points. Signal and WhatsApp use this size.
pub(crate) const DISPLAY_SIDE: f32 = 240.0;

/// Accessible name of the image. It is not the payload.
pub(crate) const QR_IMAGE_LABEL: &str = "Pairing QR code";

/// Shown when the payload does not fit in a QR code. The payload is not drawn.
pub(crate) const QR_UNDRAWN: &str = "Could not draw the code.";

/// Texture name of the Signal code.
#[cfg_attr(not(feature = "signal-local"), allow(dead_code))]
pub(crate) const SIGNAL_QR: &str = "signal-provisioning-qr";

/// Texture name of the WhatsApp code.
#[cfg_attr(not(feature = "whatsapp-web"), allow(dead_code))]
pub(crate) const WHATSAPP_QR: &str = "whatsapp-pairing-qr";

#[derive(Clone)]
struct CachedQr {
    generation: u64,
    /// Identity of the whole payload. The payload itself is not stored.
    fingerprint: u64,
    texture: TextureHandle,
}

/// Encode `payload` as one square image.
///
/// `None` when the text does not fit in a QR code. [`show_pairing_qr`] then
/// draws [`QR_UNDRAWN`]. The raw payload is not a fallback.
pub(crate) fn provisioning_qr_image(payload: &str) -> Option<egui::ColorImage> {
    let code = qrcode::QrCode::new(payload.as_bytes()).ok()?;
    let modules = code.width();
    let side = modules + QR_QUIET * 2;
    let size = side * QR_SCALE;
    let mut pixels = Vec::with_capacity(size * size);
    for y in 0..size {
        for x in 0..size {
            let mx = x / QR_SCALE;
            let my = y / QR_SCALE;
            let dark = mx >= QR_QUIET
                && my >= QR_QUIET
                && mx < QR_QUIET + modules
                && my < QR_QUIET + modules
                && code[(mx - QR_QUIET, my - QR_QUIET)] == qrcode::Color::Dark;
            pixels.push(if dark {
                egui::Color32::BLACK
            } else {
                egui::Color32::WHITE
            });
        }
    }
    Some(egui::ColorImage::new([size, size], pixels))
}

/// Whether the image was drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PairingQr {
    /// The payload fit in one code.
    Shown,
    /// The payload does not fit in a QR code. Callers must not draw the text.
    TooLarge,
}

/// Draw the cached code, centered, at [`DISPLAY_SIDE`].
///
/// The same widget is used on the Signal link screen and the WhatsApp pair
/// screen, so the two codes have the same size, quiet zone, and placement.
/// When the payload does not fit, draw [`QR_UNDRAWN`] and return
/// [`PairingQr::TooLarge`]. The payload text is not drawn.
#[must_use]
pub(crate) fn show_pairing_qr(
    ui: &mut egui::Ui,
    name: &str,
    generation: u64,
    payload: &str,
) -> PairingQr {
    let Some(texture) = cached_texture(ui, name, generation, payload) else {
        ui.label(egui::RichText::new(QR_UNDRAWN).small().weak());
        return PairingQr::TooLarge;
    };
    let width = ui.available_width();
    let inset = ((width - DISPLAY_SIDE) * 0.5).max(0.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.add_space(inset);
        ui.add(
            egui::Image::new(&texture)
                .fit_to_exact_size(egui::vec2(DISPLAY_SIDE, DISPLAY_SIDE))
                .alt_text(QR_IMAGE_LABEL),
        );
    });
    PairingQr::Shown
}

/// Drop a cached code. The next draw of that name builds it again.
pub(crate) fn forget_pairing_qr(ctx: &egui::Context, name: &str) {
    ctx.data_mut(|data| {
        data.remove::<CachedQr>(qr_id(name));
    });
}

fn cached_texture(
    ui: &mut egui::Ui,
    name: &str,
    generation: u64,
    payload: &str,
) -> Option<TextureHandle> {
    let id = qr_id(name);
    let fingerprint = fingerprint(payload);
    if let Some(cached) = ui.data(|data| data.get_temp::<CachedQr>(id))
        && cached.generation == generation
        && cached.fingerprint == fingerprint
    {
        return Some(cached.texture);
    }
    let image = provisioning_qr_image(payload)?;
    let texture = ui
        .ctx()
        .load_texture(name, image, egui::TextureOptions::NEAREST);
    ui.data_mut(|data| {
        data.insert_temp(
            id,
            CachedQr {
                generation,
                fingerprint,
                texture: texture.clone(),
            },
        );
    });
    Some(texture)
}

fn qr_id(name: &str) -> egui::Id {
    // Stable for the texture name. The same name hits the same cache entry
    // on the next frame.
    egui::Id::unique(name)
}

/// FNV-1a of every byte. A newline is part of the value, so two visual lines
/// are one payload.
fn fingerprint(payload: &str) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for byte in payload.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provisioning_uri_becomes_a_square_qr() {
        let image = provisioning_qr_image("sgnl://link-device?uuid=fixture").expect("qr");
        assert_eq!(image.size[0], image.size[1]);
        assert!(image.size[0] >= (21 + QR_QUIET * 2) * QR_SCALE);
        assert!(image.pixels.contains(&egui::Color32::BLACK));
        assert!(image.pixels.contains(&egui::Color32::WHITE));
    }

    /// A payload that wraps as several `2@` lines is still one code.
    #[test]
    fn a_wrapped_payload_is_one_qr_code() {
        let payload = "2@alpha-line-of-the-same-code\n2@beta-line-of-the-same-code";
        let whole = provisioning_qr_image(payload).expect("one code");
        let first_line = provisioning_qr_image("2@alpha-line-of-the-same-code").expect("line");
        assert_eq!(whole.size[0], whole.size[1]);
        assert_ne!(
            whole.pixels, first_line.pixels,
            "the second line is part of the same code"
        );
        assert_ne!(
            fingerprint(payload),
            fingerprint("2@alpha-line-of-the-same-code")
        );
    }

    #[test]
    fn a_payload_longer_than_a_qr_code_is_not_an_image() {
        let mut payload = String::from("2@");
        payload.extend(std::iter::repeat_n('a', 8_000));
        assert!(provisioning_qr_image(&payload).is_none());
    }

    #[test]
    fn the_texture_is_reused_until_the_generation_or_the_payload_changes() {
        use egui_kittest::Harness;

        struct State {
            generation: u64,
            payload: &'static str,
        }

        let mut harness = Harness::builder()
            .with_size(egui::vec2(320.0, 320.0))
            .build_ui_state(
                |ui, state: &mut State| {
                    let _ = show_pairing_qr(ui, "qr-cache", state.generation, state.payload);
                },
                State {
                    generation: 1,
                    payload: "2@one\n2@two",
                },
            );
        harness.run_steps(2);
        let first = cached_texture_id(&harness.ctx, "qr-cache").expect("texture");
        harness.run_steps(2);
        assert_eq!(
            cached_texture_id(&harness.ctx, "qr-cache"),
            Some(first),
            "a frame does not build the image again"
        );

        harness.state_mut().payload = "2@one\n2@two-rotated";
        harness.run_steps(1);
        let rotated = cached_texture_id(&harness.ctx, "qr-cache").expect("rotated");
        assert_ne!(rotated, first, "a new payload of the same generation");

        harness.state_mut().generation = 2;
        harness.state_mut().payload = "2@one\n2@two";
        harness.run_steps(1);
        let next = cached_texture_id(&harness.ctx, "qr-cache").expect("generation");
        assert_ne!(next, rotated);
        assert_ne!(next, first, "a new pairing generation builds a new image");

        forget_pairing_qr(&harness.ctx, "qr-cache");
        assert_eq!(cached_texture_id(&harness.ctx, "qr-cache"), None);
    }

    /// A payload that does not fit in a QR code draws a short line. The
    /// payload text is not a fallback.
    #[test]
    fn a_payload_that_does_not_fit_draws_a_line_and_not_the_text() {
        use eframe::egui::accesskit::Role;
        use egui_kittest::Harness;
        use egui_kittest::kittest::{By, Queryable};

        let payload = "2@not-drawn-payload-".repeat(300);
        assert!(
            provisioning_qr_image(&payload).is_none(),
            "the fixture is too long for a QR code"
        );
        let mut harness = Harness::builder()
            .with_size(egui::vec2(320.0, 200.0))
            .build_ui_state(
                |ui, payload: &mut String| {
                    assert_eq!(
                        show_pairing_qr(ui, "qr-too-big", 1, payload),
                        PairingQr::TooLarge
                    );
                },
                payload,
            );
        harness.run_steps(2);
        harness.get_by_label(QR_UNDRAWN);
        assert!(
            harness
                .query_by_label_contains("not-drawn-payload")
                .is_none()
        );
        assert!(harness.query_by_label_contains("2@").is_none());
        assert!(
            harness
                .query_all(By::new().role(Role::Image))
                .next()
                .is_none(),
            "no image when the code does not fit"
        );
    }

    fn cached_texture_id(ctx: &egui::Context, name: &str) -> Option<egui::TextureId> {
        ctx.data(|data| {
            data.get_temp::<CachedQr>(qr_id(name))
                .map(|cached| cached.texture.id())
        })
    }
}
