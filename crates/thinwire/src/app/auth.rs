//! Non-modal Telegram login. Cancel is always available. Secrets stay off disk
//! except in the OS keychain (or in-memory when the keychain is unavailable).

use eframe::egui::{self, Color32, RichText};

use super::secrets::SecretStore;
use super::snapshot::{AuthScreen, Snapshot};

const MUTED: Color32 = Color32::from_rgb(160, 160, 168);
const WARN: Color32 = Color32::from_rgb(214, 160, 64);

/// Shown on every Telegram auth screen. Must stay on-screen, not README-only.
pub(crate) const TELEGRAM_STUB_BANNER: &str = "Stub — no live TDLib session yet. These screens advance locally. Do not paste api_id or api_hash expecting a real Telegram login.";

pub(crate) fn draw(ui: &mut egui::Ui, snapshot: &mut Snapshot, secrets: &SecretStore) {
    if snapshot.auth == AuthScreen::Idle {
        return;
    }

    ui.separator();
    ui.heading("Add Telegram account (TDLib stub)");
    stub_banner(ui);
    ui.label(
        RichText::new(
            "Cancel is always available. This UI does not block the UI thread. If you still enter values, they go to the OS keychain when one is available and are never written to the git repo or logged.",
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

fn stub_banner(ui: &mut egui::Ui) {
    ui.colored_label(WARN, TELEGRAM_STUB_BANNER);
}

fn telegram_api(ui: &mut egui::Ui, snapshot: &mut Snapshot, secrets: &SecretStore) {
    ui.label(
        "Help: https://my.telegram.org — API development tools. This step does not start TDLib.",
    );
    ui.label("1. Open my.telegram.org, sign in, and open API development tools.");
    ui.label("2. Optional practice only: copy api_id and api_hash. Nothing is sent to Telegram.");
    ui.label(
        RichText::new(
            "Stub step. Values, if entered, stay in the OS keychain or memory. Not a live login.",
        )
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
    if ui.button("Continue (stub)").clicked() {
        snapshot.advance_telegram(secrets);
    }
}

fn telegram_phone(ui: &mut egui::Ui, snapshot: &mut Snapshot, secrets: &SecretStore) {
    ui.label("Phone number practice field. No code is sent. No live TDLib session.");
    ui.label(
        RichText::new(
            "Stub step. Enabling feature telegram-tdlib later uses this same screen to request a real code.",
        )
        .small()
        .color(MUTED),
    );
    ui.add(egui::TextEdit::singleline(&mut snapshot.telegram_phone).hint_text("+15551234567"));
    if ui.button("Send code (stub)").clicked() {
        snapshot.advance_telegram(secrets);
    }
}

fn telegram_code(ui: &mut egui::Ui, snapshot: &mut Snapshot, secrets: &SecretStore) {
    ui.label("Login code practice field. Telegram did not send a code. No live TDLib session.");
    ui.add(
        egui::TextEdit::singleline(&mut snapshot.telegram_code)
            .password(true)
            .hint_text("unused in this stub"),
    );
    if ui.button("Continue (stub)").clicked() {
        snapshot.advance_telegram(secrets);
    }
}

fn telegram_2fa(ui: &mut egui::Ui, snapshot: &mut Snapshot, secrets: &SecretStore) {
    ui.label("Optional 2FA practice field. Leave blank to skip. No live TDLib session.");
    ui.add(
        egui::TextEdit::singleline(&mut snapshot.telegram_2fa)
            .password(true)
            .hint_text("unused in this stub"),
    );
    if ui.button("Finish (stub)").clicked() {
        snapshot.advance_telegram(secrets);
    }
}
