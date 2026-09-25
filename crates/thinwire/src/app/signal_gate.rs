//! Full-screen local-only Signal notice.
//!
//! Compiled only with `signal-local`. The provisioning URL is unreachable
//! until the notice is accepted. Release builds do not include this screen.

use eframe::egui::{self, RichText};

use super::theme;
use thinwire_core::state::{SignalScreen, Snapshot};
use thinwire_core::{Intent, SignalIntent, View};

pub(crate) fn notice_entry(ui: &mut egui::Ui, out: &mut Vec<Intent>) {
    ui.add_space(8.0);
    ui.label(
        RichText::new("Signal — local build only, not in releases")
            .small()
            .color(theme::palette(ui).warn),
    );
    if ui.button("Review Signal local-build notice").clicked() {
        out.push(Intent::Signal(SignalIntent::OpenNotice));
        ui.ctx().request_repaint();
    }
}

pub(crate) fn draw(ui: &mut egui::Ui, snapshot: &View<'_>, out: &mut Vec<Intent>) {
    egui::CentralPanel::default().show(ui, |ui| {
        egui::ScrollArea::vertical().show(ui, |ui| match snapshot.signal_screen {
            SignalScreen::Hidden => {}
            SignalScreen::Notice => notice(ui, out),
            SignalScreen::Link => link_screen(ui, snapshot, out),
        });
    });
}

fn notice(ui: &mut egui::Ui, out: &mut Vec<Intent>) {
    ui.heading("Signal local build");
    ui.colored_label(
        theme::palette(ui).warn,
        "This build links AGPL presage and libsignal. It is experimental and local only. Release builds and OS zips do not include Signal.",
    );
    ui.add_space(8.0);
    ui.label("No provisioning URL is shown on this screen.");
    ui.add_space(12.0);
    if ui
        .button("I understand — this is a local build, not a release")
        .clicked()
    {
        out.push(Intent::Signal(SignalIntent::AcknowledgeNotice));
    }
    if ui.button("Back").clicked() {
        out.push(Intent::Signal(SignalIntent::CloseGate));
    }
}

fn link_screen(ui: &mut egui::Ui, snapshot: &Snapshot, out: &mut Vec<Intent>) {
    ui.heading("Link a secondary device");
    ui.label(
        "The worker stays idle until you start it. The provisioning URL is not placed on the command channel.",
    );
    ui.add_space(8.0);
    if let Some(error) = &snapshot.error {
        ui.colored_label(theme::palette(ui).warn, &error.happened);
        ui.label(&error.why);
        ui.label(&error.next);
        ui.add_space(8.0);
    }
    if ui
        .add_enabled(
            !snapshot.signal_started,
            egui::Button::new("Start Signal linking"),
        )
        .clicked()
    {
        out.push(Intent::Signal(SignalIntent::BeginLink));
    }
    if let Some(qr) = &snapshot.signal_qr {
        ui.add_space(8.0);
        ui.colored_label(
            theme::palette(ui).warn,
            "Provisioning URL. Do not commit it, log it, or paste it into a ticket.",
        );
        if let Some(image) = provisioning_qr_image(qr) {
            let texture = ui.ctx().load_texture(
                "signal-provisioning-qr",
                image,
                egui::TextureOptions::NEAREST,
            );
            ui.add(egui::Image::new(&texture).fit_to_exact_size(egui::vec2(240.0, 240.0)));
            ui.add_space(8.0);
        }
        ui.monospace(qr);
    }
    ui.add_space(12.0);
    ui.label(
        RichText::new("Cancel returns to the shell and stops the worker task.")
            .small()
            .weak(),
    );
    if ui.button("Cancel linking").clicked() {
        out.push(Intent::Signal(SignalIntent::CancelLink));
    }
}

/// QR modules plus a quiet zone, scaled so a phone camera can read the URI.
const QR_SCALE: usize = 8;
const QR_QUIET: usize = 4;

fn provisioning_qr_image(uri: &str) -> Option<egui::ColorImage> {
    let code = qrcode::QrCode::new(uri.as_bytes()).ok()?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provisioning_uri_becomes_a_square_qr() {
        let image = provisioning_qr_image("sgnl://link-device?uuid=fixture").expect("qr");
        assert_eq!(image.size[0], image.size[1]);
        assert!(image.size[0] >= (21 + QR_QUIET * 2) * QR_SCALE);
        assert!(
            image
                .pixels
                .iter()
                .any(|pixel| *pixel == egui::Color32::BLACK)
        );
        assert!(
            image
                .pixels
                .iter()
                .any(|pixel| *pixel == egui::Color32::WHITE)
        );
    }
}
