//! Non-modal Telegram login. Cancel is always available. Secrets stay off disk
//! except in the OS keychain (or in-memory when the keychain is unavailable).

use eframe::egui::{self, Color32, RichText};

use super::secrets::SecretStore;
use super::snapshot::{AuthScreen, Snapshot};

const MUTED: Color32 = Color32::from_rgb(160, 160, 168);
const WARN: Color32 = Color32::from_rgb(214, 160, 64);

/// Shown only when the live `tdlib-rs` client was not compiled in.
pub(crate) const TDLIB_UNAVAILABLE_BANNER: &str = "TDLib unavailable in this build. Enable feature telegram-tdlib after a local TDLib install. These screens do not open a live Telegram session.";

#[must_use]
pub(crate) const fn tdlib_compiled() -> bool {
    cfg!(feature = "telegram-tdlib")
}

pub(crate) fn draw(ui: &mut egui::Ui, snapshot: &mut Snapshot, secrets: &SecretStore) {
    if snapshot.auth == AuthScreen::Idle {
        return;
    }

    ui.separator();
    ui.heading(if tdlib_compiled() {
        "Add Telegram account"
    } else {
        "Add Telegram account (TDLib unavailable)"
    });
    if !tdlib_compiled() {
        ui.colored_label(WARN, TDLIB_UNAVAILABLE_BANNER);
    }
    ui.label(
        RichText::new(
            "Cancel is always available. This UI does not block the UI thread. Credentials go to the secret store and are never written to the git repo or logged.",
        )
        .small()
        .color(MUTED),
    );
    if snapshot.auth_busy {
        ui.label(
            RichText::new("Waiting for the Telegram adapter. The UI thread stays free.")
                .small()
                .color(MUTED),
        );
    }

    match snapshot.auth {
        AuthScreen::Idle => {}
        AuthScreen::TelegramApi => telegram_api(ui, snapshot, secrets),
        AuthScreen::TelegramPhone => telegram_phone(ui, snapshot, secrets),
        AuthScreen::TelegramCode => telegram_code(ui, snapshot, secrets),
        AuthScreen::Telegram2fa => telegram_2fa(ui, snapshot, secrets),
    }

    ui.add_space(8.0);
    if ui.button("Cancel").clicked() {
        snapshot.cancel_auth(secrets);
    }
}

fn telegram_api(ui: &mut egui::Ui, snapshot: &mut Snapshot, secrets: &SecretStore) {
    ui.label("Help: https://my.telegram.org — API development tools.");
    ui.label("1. Open my.telegram.org, sign in, and open API development tools.");
    ui.label("2. Copy api_id and api_hash. They stay in the secret store.");
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
    continue_button(ui, snapshot, secrets, "Continue");
}

fn telegram_phone(ui: &mut egui::Ui, snapshot: &mut Snapshot, secrets: &SecretStore) {
    ui.label("Phone number in international format. It stays in the secret store.");
    ui.add(egui::TextEdit::singleline(&mut snapshot.telegram_phone).hint_text("+15551234567"));
    continue_button(ui, snapshot, secrets, "Send code");
}

fn telegram_code(ui: &mut egui::Ui, snapshot: &mut Snapshot, secrets: &SecretStore) {
    ui.label("Login code. It stays in the secret store and is not logged.");
    ui.add(
        egui::TextEdit::singleline(&mut snapshot.telegram_code)
            .password(true)
            .hint_text("login code"),
    );
    continue_button(ui, snapshot, secrets, "Continue");
}

fn telegram_2fa(ui: &mut egui::Ui, snapshot: &mut Snapshot, secrets: &SecretStore) {
    ui.label("Optional 2FA password. Leave blank to skip if this account has none.");
    ui.add(
        egui::TextEdit::singleline(&mut snapshot.telegram_2fa)
            .password(true)
            .hint_text("optional"),
    );
    continue_button(ui, snapshot, secrets, "Finish");
}

fn continue_button(ui: &mut egui::Ui, snapshot: &mut Snapshot, secrets: &SecretStore, label: &str) {
    ui.add_enabled_ui(!snapshot.auth_busy, |ui| {
        if ui.button(label).clicked() {
            snapshot.advance_telegram(secrets);
        }
    });
}
