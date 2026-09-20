//! Non-modal Telegram login. Cancel is always available. Secrets stay off disk
//! except in the OS keychain (or in-memory when the keychain is unavailable).

use eframe::egui::{self, Color32, RichText};

use super::secrets::SecretStore;
use super::snapshot::{AuthScreen, Snapshot};

const MUTED: Color32 = Color32::from_rgb(160, 160, 168);

pub(crate) fn draw(ui: &mut egui::Ui, snapshot: &mut Snapshot, secrets: &SecretStore) {
    if snapshot.auth == AuthScreen::Idle {
        return;
    }

    ui.separator();
    ui.heading("Add Telegram account");
    ui.label(
        RichText::new(
            "Cancel is always available. This UI does not block the UI thread. api_id, api_hash, and session material go to the OS keychain when one is available; they are never written to the git repo or logged.",
        )
        .small()
        .color(MUTED),
    );

    match snapshot.auth {
        AuthScreen::Idle => {}
        AuthScreen::TelegramApi => telegram_api(ui, snapshot, secrets),
        AuthScreen::TelegramPhone => telegram_phone(ui, snapshot, secrets),
        AuthScreen::TelegramCode => telegram_code(ui, snapshot, secrets),
        AuthScreen::Telegram2fa => telegram_2fa(ui, snapshot, secrets),
    }

    ui.add_space(8.0);
    if ui.button("Cancel").clicked() {
        snapshot.cancel_auth();
    }
}

fn telegram_api(ui: &mut egui::Ui, snapshot: &mut Snapshot, secrets: &SecretStore) {
    ui.label("1. Open my.telegram.org, sign in, and open API development tools.");
    ui.label("2. Create an application and copy api_id and api_hash into these fields.");
    ui.label(
        RichText::new("Help: https://my.telegram.org — values are stored in the OS keychain, not in the repo.")
            .small()
            .color(MUTED),
    );
    ui.horizontal(|ui| {
        ui.label("api_id");
        ui.add(
            egui::TextEdit::singleline(&mut snapshot.telegram_api_id)
                .hint_text("numeric id from my.telegram.org"),
        );
    });
    ui.horizontal(|ui| {
        ui.label("api_hash");
        ui.add(
            egui::TextEdit::singleline(&mut snapshot.telegram_api_hash)
                .password(true)
                .hint_text("from my.telegram.org"),
        );
    });
    if ui.button("Continue").clicked() {
        snapshot.advance_telegram(secrets);
    }
}

fn telegram_phone(ui: &mut egui::Ui, snapshot: &mut Snapshot, secrets: &SecretStore) {
    ui.label("Phone number, including country code. The number stays on this machine.");
    ui.label(
        RichText::new(
            "This build does not start TDLib. Enabling feature telegram-tdlib later uses this same screen to request a code.",
        )
        .small()
        .color(MUTED),
    );
    ui.add(egui::TextEdit::singleline(&mut snapshot.telegram_phone).hint_text("+15551234567"));
    if ui.button("Send code").clicked() {
        snapshot.advance_telegram(secrets);
    }
}

fn telegram_code(ui: &mut egui::Ui, snapshot: &mut Snapshot, secrets: &SecretStore) {
    ui.label("Login code from Telegram. It is not written to disk.");
    ui.add(
        egui::TextEdit::singleline(&mut snapshot.telegram_code)
            .password(true)
            .hint_text("login code"),
    );
    if ui.button("Continue").clicked() {
        snapshot.advance_telegram(secrets);
    }
}

fn telegram_2fa(ui: &mut egui::Ui, snapshot: &mut Snapshot, secrets: &SecretStore) {
    ui.label("Optional 2FA password. Leave blank to skip. Never committed or logged.");
    ui.add(
        egui::TextEdit::singleline(&mut snapshot.telegram_2fa)
            .password(true)
            .hint_text("2FA password (optional)"),
    );
    if ui.button("Finish").clicked() {
        snapshot.advance_telegram(secrets);
    }
}
