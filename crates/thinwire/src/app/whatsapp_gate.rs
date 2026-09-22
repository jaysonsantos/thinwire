//! Full-screen experimental WhatsApp gate.
//!
//! Compiled only with `whatsapp-web`. The QR and pair screen is unreachable
//! until the ban acknowledgement. This is not a supported messenger.

use eframe::egui::{self, Color32, RichText};
use thinwire_protocol::{CRITIC_RISK_BULLETS, WhatsAppPhoneVault};

use super::snapshot::{Snapshot, WhatsAppScreen};

const WARN: Color32 = Color32::from_rgb(214, 160, 64);
const MUTED: Color32 = Color32::from_rgb(160, 160, 168);

pub(crate) fn risk_entry(ui: &mut egui::Ui, snapshot: &mut Snapshot) {
    ui.add_space(8.0);
    ui.label(
        RichText::new("WhatsApp spike — experimental, ban risk")
            .small()
            .color(WARN),
    );
    if ui.button("Review WhatsApp ban risk").clicked() {
        snapshot.open_whatsapp_risk_gate();
        ui.ctx().request_repaint();
    }
}

pub(crate) fn draw(ui: &mut egui::Ui, snapshot: &mut Snapshot, phone: &WhatsAppPhoneVault) {
    egui::CentralPanel::default().show(ui, |ui| {
        egui::ScrollArea::vertical().show(ui, |ui| match snapshot.whatsapp_screen {
            WhatsAppScreen::Hidden => {}
            WhatsAppScreen::RiskGate => risk_gate(ui, snapshot),
            WhatsAppScreen::Pair => pair_screen(ui, snapshot, phone),
        });
    });
}

fn risk_gate(ui: &mut egui::Ui, snapshot: &mut Snapshot) {
    ui.heading("WhatsApp experimental spike");
    ui.colored_label(
        WARN,
        "Unofficial linked-device pairing can get a personal account banned. This is not a supported messenger.",
    );
    ui.add_space(8.0);
    for (index, bullet) in CRITIC_RISK_BULLETS.iter().enumerate() {
        ui.label(format!("{}. {bullet}", index + 1));
        ui.add_space(6.0);
    }
    ui.label("No QR code and no pair code are shown on this screen.");
    ui.add_space(12.0);
    if ui
        .button("I understand — this can ban my account")
        .clicked()
    {
        snapshot.acknowledge_whatsapp_risk();
    }
    if ui.button("Back").clicked() {
        snapshot.close_whatsapp_gate();
    }
}

fn pair_screen(ui: &mut egui::Ui, snapshot: &mut Snapshot, phone: &WhatsAppPhoneVault) {
    ui.heading("Experimental pairing");
    ui.label(
        "The worker stays idle until you start it. A phone number stays in memory and is not placed on the command channel.",
    );
    ui.add_space(8.0);
    ui.label("Optional phone for a pair code (digits). Leave blank for a QR payload only.");
    ui.add(
        egui::TextEdit::singleline(&mut snapshot.whatsapp_phone)
            .hint_text("15551234567")
            .desired_width(240.0),
    );
    if ui
        .add_enabled(
            !snapshot.whatsapp_started,
            egui::Button::new("Start experimental pairing"),
        )
        .clicked()
    {
        snapshot.begin_whatsapp_link(phone);
    }
    if let Some(qr) = &snapshot.whatsapp_qr {
        ui.add_space(8.0);
        ui.colored_label(
            WARN,
            "QR payload. Do not commit it, log it, or paste it into a ticket.",
        );
        ui.monospace(qr);
    }
    if let Some(code) = &snapshot.whatsapp_pair_code {
        ui.add_space(8.0);
        ui.label("Pair code from the worker. Do not commit it or log it.");
        ui.monospace(code);
    }
    ui.add_space(12.0);
    ui.label(
        RichText::new("Cancel returns to the shell and stops the worker task.")
            .small()
            .color(MUTED),
    );
    if ui.button("Cancel pairing").clicked() {
        snapshot.cancel_whatsapp_link(phone);
    }
}
