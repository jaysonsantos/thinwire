//! Full-screen experimental Signal notice.
//!
//! Compiled only with `signal-local`. The provisioning URL is unreachable
//! until the notice is accepted. The client runs in the Signal helper
//! process (ADR 0013). The helper starts after that acceptance. A saved
//! acceptance of the same notice copy starts the helper at the next launch.

use eframe::egui::{self, RichText};

use super::theme;
use thinwire_core::signal_notice::{
    SIGNAL_NOTICE_ACCEPT, SIGNAL_NOTICE_BODY, SIGNAL_NOTICE_HEADING, SIGNAL_NOTICE_HELPER,
    SIGNAL_NOTICE_NO_URL, SIGNAL_RISK,
};
use thinwire_core::state::{SignalScreen, Snapshot};
use thinwire_core::{Intent, ProtocolId, SignalIntent, View};

/// The Signal entry under the accounts. With no helper program the entry
/// stays, disabled, with the reason (ADR 0013 decision 6).
pub(crate) fn notice_entry(ui: &mut egui::Ui, snapshot: &Snapshot, out: &mut Vec<Intent>) {
    ui.add_space(8.0);
    ui.label(
        RichText::new("Signal — experimental, expect breakage")
            .small()
            .color(theme::palette(ui).warn),
    );
    let missing = snapshot.helper_missing(ProtocolId::Signal);
    let entry = ui.add_enabled(!missing, egui::Button::new("Review Signal notice"));
    if missing {
        entry.on_disabled_hover_text(HELPER_MISSING_REASON);
        ui.label(RichText::new(HELPER_MISSING_REASON).small().weak());
    } else if entry.clicked() {
        out.push(Intent::Signal(SignalIntent::OpenNotice));
        ui.ctx().request_repaint();
    }
}

/// Why the Signal entry is disabled.
const HELPER_MISSING_REASON: &str = "Needs the Signal helper. Reinstall thinwire.";

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
    ui.heading(SIGNAL_NOTICE_HEADING);
    ui.colored_label(theme::palette(ui).warn, SIGNAL_RISK);
    ui.add_space(8.0);
    ui.label(SIGNAL_NOTICE_BODY);
    ui.add_space(6.0);
    ui.label(SIGNAL_NOTICE_HELPER);
    ui.add_space(8.0);
    ui.label(SIGNAL_NOTICE_NO_URL);
    ui.add_space(12.0);
    if ui.button(SIGNAL_NOTICE_ACCEPT).clicked() {
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
    ui.colored_label(
        theme::palette(ui).warn,
        "This build stores the Signal session in SQLite. A session saved by the previous sled store cannot be opened. Link this device again. The old files stay on disk.",
    );
    ui.add_space(8.0);
    if let Some(error) = &snapshot.error {
        ui.colored_label(theme::palette(ui).warn, &error.happened);
        ui.label(&error.why);
        ui.label(&error.next);
        ui.add_space(8.0);
    }
    // With no helper program no link can start. The core refuses it too.
    // Show the reason next to the disabled button.
    let missing = snapshot.helper_missing(ProtocolId::Signal);
    if missing {
        ui.colored_label(theme::palette(ui).warn, HELPER_MISSING_REASON);
    }
    if ui
        .add_enabled(
            !snapshot.signal_started && !missing,
            egui::Button::new("Start Signal linking"),
        )
        .clicked()
    {
        out.push(Intent::Signal(SignalIntent::BeginLink));
    }
    // This screen draws no account row. Show the helper state here: a start
    // that takes long, a restart, or a helper that stopped, with what to do.
    if let Some(notice) = snapshot.helper_notice(ProtocolId::Signal) {
        ui.add_space(8.0);
        ui.colored_label(theme::palette(ui).warn, notice.label);
        if let Some(error) = notice.error {
            ui.label(format!("What happened: {}", error.happened));
            ui.label(format!("Why: {}", error.why));
            ui.label(format!("What to do: {}", error.next));
            ui.label("You can also start the link again here.");
        }
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
        assert!(image.pixels.contains(&egui::Color32::BLACK));
        assert!(image.pixels.contains(&egui::Color32::WHITE));
    }
}
