//! Non-modal auth screens. Field values stay in the snapshot and are discarded.

use eframe::egui::{self, Color32, RichText};
use thinwire_protocol::{DiscordAuthMode, ProtocolId};

use super::snapshot::{AuthScreen, Snapshot};

const MUTED: Color32 = Color32::from_rgb(160, 160, 168);
const WARN: Color32 = Color32::from_rgb(214, 160, 64);

pub(crate) fn draw(ui: &mut egui::Ui, snapshot: &mut Snapshot) {
    if snapshot.auth == AuthScreen::Idle {
        return;
    }

    ui.separator();
    ui.heading("Add account");
    ui.label(
        RichText::new("Cancel is always available. This UI does not block the UI thread and does not store secrets.")
            .small()
            .color(MUTED),
    );

    match snapshot.auth {
        AuthScreen::Idle => {}
        AuthScreen::ChooseProtocol => choose_protocol(ui, snapshot),
        AuthScreen::ExperimentalGate { protocol } => gate(ui, snapshot, protocol),
        AuthScreen::TelegramApi => telegram_api(ui, snapshot),
        AuthScreen::TelegramPhone => telegram_phone(ui, snapshot),
        AuthScreen::TelegramCode => telegram_code(ui, snapshot),
        AuthScreen::Telegram2fa => telegram_2fa(ui, snapshot),
        AuthScreen::WhatsAppQr => whatsapp_qr(ui, snapshot),
        AuthScreen::SignalLink => signal_link(ui, snapshot),
        AuthScreen::DiscordChoose => discord_choose(ui, snapshot),
        AuthScreen::DiscordBot => discord_bot(ui, snapshot),
        AuthScreen::DiscordOAuth => discord_oauth(ui, snapshot),
        AuthScreen::SlackOAuth => slack_oauth(ui, snapshot),
    }

    ui.add_space(8.0);
    if ui.button("Cancel").clicked() {
        snapshot.cancel_auth();
    }
}

fn choose_protocol(ui: &mut egui::Ui, snapshot: &mut Snapshot) {
    ui.label("Supported first. Experimental modules sit behind a risk gate.");
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if ui.button("Telegram · Supported · TDLib").clicked() {
            snapshot.start_supported(ProtocolId::Telegram);
        }
        if ui.button("Slack · Supported · OAuth").clicked() {
            snapshot.start_supported(ProtocolId::Slack);
        }
    });
    ui.add_space(6.0);
    ui.label(RichText::new("Experimental / constrained").color(WARN));
    ui.horizontal(|ui| {
        if ui.button("WhatsApp · Experimental · ban risk").clicked() {
            snapshot.choose_protocol(ProtocolId::WhatsApp);
        }
        if ui.button("Signal · Experimental").clicked() {
            snapshot.choose_protocol(ProtocolId::Signal);
        }
        if ui.button("Discord · Constrained · bot/OAuth").clicked() {
            snapshot.choose_protocol(ProtocolId::Discord);
        }
    });
}

fn gate(ui: &mut egui::Ui, snapshot: &mut Snapshot, protocol: ProtocolId) {
    ui.colored_label(
        WARN,
        format!(
            "{} is experimental or constrained. Read this before any QR, code, or token step.",
            protocol.display_name()
        ),
    );
    ui.add_space(4.0);
    for (index, bullet) in snapshot.critic_lines().iter().enumerate() {
        ui.label(format!("{}. {bullet}", index + 1));
        ui.add_space(4.0);
    }
    ui.checkbox(&mut snapshot.risk_understood, "I understand the risk");
    ui.add_enabled_ui(snapshot.risk_understood, |ui| {
        if ui.button("Continue").clicked() {
            let _ = snapshot.continue_experimental();
        }
    });
}

fn telegram_api(ui: &mut egui::Ui, snapshot: &mut Snapshot) {
    ui.label("1. Open my.telegram.org, sign in, and create an application.");
    ui.label("2. Copy api_id and api_hash into these fields. They are not written to disk.");
    ui.horizontal(|ui| {
        ui.label("api_id");
        ui.text_edit_singleline(&mut snapshot.telegram_api_id);
    });
    ui.horizontal(|ui| {
        ui.label("api_hash");
        ui.text_edit_singleline(&mut snapshot.telegram_api_hash);
    });
    if ui.button("Continue").clicked() {
        snapshot.advance_telegram();
    }
}

fn telegram_phone(ui: &mut egui::Ui, snapshot: &mut Snapshot) {
    ui.label("Phone number (local only). TDLib is not started in this build.");
    ui.text_edit_singleline(&mut snapshot.telegram_phone);
    if ui.button("Send code (stub)").clicked() {
        snapshot.advance_telegram();
    }
}

fn telegram_code(ui: &mut egui::Ui, snapshot: &mut Snapshot) {
    ui.label("Login code (local only).");
    ui.text_edit_singleline(&mut snapshot.telegram_code);
    if ui.button("Continue").clicked() {
        snapshot.advance_telegram();
    }
}

fn telegram_2fa(ui: &mut egui::Ui, snapshot: &mut Snapshot) {
    ui.label("Optional 2FA password. Leave blank to skip. Never committed.");
    ui.text_edit_singleline(&mut snapshot.telegram_2fa);
    if ui.button("Finish stub").clicked() {
        snapshot.advance_telegram();
    }
}

fn whatsapp_qr(ui: &mut egui::Ui, snapshot: &mut Snapshot) {
    ui.colored_label(WARN, "Experimental · ban risk");
    ui.label("QR placeholder. On a phone: Linked devices → Link a device.");
    ui.group(|ui| {
        ui.label(RichText::new("[ QR placeholder ]").italics().color(MUTED));
        ui.label("No linked-device session is open.");
    });
    if ui.button("Close stub").clicked() {
        snapshot.finish_whatsapp_placeholder();
    }
}

fn signal_link(ui: &mut egui::Ui, snapshot: &mut Snapshot) {
    ui.colored_label(WARN, "Experimental. Breakage expected.");
    ui.label("Link QR/code placeholder. Signal has no supported third-party client API.");
    ui.group(|ui| {
        ui.label(
            RichText::new("[ link code placeholder ]")
                .italics()
                .color(MUTED),
        );
    });
    if ui.button("Close stub").clicked() {
        snapshot.finish_signal_placeholder();
    }
}

fn discord_choose(ui: &mut egui::Ui, snapshot: &mut Snapshot) {
    ui.label(
        "Discord is bot/OAuth only. User login can ban an account. No user-token field exists.",
    );
    ui.horizontal(|ui| {
        if ui.button("Bot stub").clicked() {
            snapshot.open_discord_bot();
        }
        if ui.button("OAuth stub").clicked() {
            snapshot.open_discord_oauth();
        }
    });
}

fn discord_bot(ui: &mut egui::Ui, snapshot: &mut Snapshot) {
    ui.label("Bot application stub. Paste nothing here in this revision.");
    if ui.button("Finish bot stub").clicked() {
        snapshot.finish_discord(DiscordAuthMode::Bot);
    }
}

fn discord_oauth(ui: &mut egui::Ui, snapshot: &mut Snapshot) {
    ui.label("OAuth application stub. Browser sign-in is not started.");
    if ui.button("Finish OAuth stub").clicked() {
        snapshot.finish_discord(DiscordAuthMode::OAuth);
    }
}

fn slack_oauth(ui: &mut egui::Ui, snapshot: &mut Snapshot) {
    ui.label("Slack workspace OAuth. Browser sign-in would open next. Not a personal Slack desktop clone.");
    if ui.button("Finish OAuth stub").clicked() {
        snapshot.finish_slack();
    }
}
