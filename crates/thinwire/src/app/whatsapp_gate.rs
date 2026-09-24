//! Full-screen experimental WhatsApp gate.
//!
//! Compiled only with `whatsapp-web`. The QR and pair screen is unreachable
//! until the ban acknowledgement. This is not a supported messenger.

use eframe::egui::{self, RichText};
use thinwire_protocol::CRITIC_RISK_BULLETS;

use super::theme;
use super::ui::edited;
use thinwire_core::state::{Snapshot, WhatsAppScreen};
use thinwire_core::{Intent, SecretText, View, WhatsAppIntent};

pub(crate) fn risk_entry(ui: &mut egui::Ui, out: &mut Vec<Intent>) {
    ui.add_space(8.0);
    ui.label(
        RichText::new("WhatsApp spike — experimental, ban risk")
            .small()
            .color(theme::palette(ui).warn),
    );
    if ui.button("Review WhatsApp ban risk").clicked() {
        out.push(Intent::WhatsApp(WhatsAppIntent::OpenRiskGate));
        ui.ctx().request_repaint();
    }
}

pub(crate) fn draw(ui: &mut egui::Ui, snapshot: &View<'_>, out: &mut Vec<Intent>) {
    egui::CentralPanel::default().show(ui, |ui| {
        egui::ScrollArea::vertical().show(ui, |ui| match snapshot.whatsapp_screen {
            WhatsAppScreen::Hidden => {}
            WhatsAppScreen::RiskGate => risk_gate(ui, out),
            WhatsAppScreen::Pair => pair_screen(ui, snapshot, out),
        });
    });
}

fn risk_gate(ui: &mut egui::Ui, out: &mut Vec<Intent>) {
    ui.heading("WhatsApp experimental spike");
    ui.colored_label(
        theme::palette(ui).warn,
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
        out.push(Intent::WhatsApp(WhatsAppIntent::AcknowledgeRisk));
    }
    if ui.button("Back").clicked() {
        out.push(Intent::WhatsApp(WhatsAppIntent::CloseGate));
    }
}

fn pair_screen(ui: &mut egui::Ui, snapshot: &Snapshot, out: &mut Vec<Intent>) {
    ui.heading("Experimental pairing");
    ui.label(
        "The worker stays idle until you start it. A phone number stays in memory and is not placed on the command channel.",
    );
    ui.add_space(8.0);
    ui.label("Optional phone for a pair code (digits). Leave blank for a QR payload only.");
    if let Some(value) = edited(ui, &snapshot.whatsapp_phone, |text| {
        egui::TextEdit::singleline(text)
            .hint_text("15551234567")
            .desired_width(240.0)
    }) {
        out.push(Intent::WhatsApp(WhatsAppIntent::SetPhone(SecretText::new(
            value,
        ))));
    }
    if ui
        .add_enabled(
            !snapshot.whatsapp_started,
            egui::Button::new("Start experimental pairing"),
        )
        .clicked()
    {
        out.push(Intent::WhatsApp(WhatsAppIntent::BeginLink));
    }
    if let Some(qr) = &snapshot.whatsapp_qr {
        ui.add_space(8.0);
        ui.colored_label(
            theme::palette(ui).warn,
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
            .weak(),
    );
    if ui.button("Cancel pairing").clicked() {
        out.push(Intent::WhatsApp(WhatsAppIntent::CancelLink));
    }
}
