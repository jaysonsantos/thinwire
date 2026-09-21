//! Non-modal Telegram login. Cancel is always available. Secrets stay off disk
//! except in the OS keychain (or in-memory when the keychain is unavailable).

use eframe::egui::{self, Color32, RichText};

use super::secrets::SecretStore;
use super::snapshot::{AuthScreen, Snapshot};

const MUTED: Color32 = Color32::from_rgb(160, 160, 168);
const WARN: Color32 = Color32::from_rgb(214, 160, 64);

/// Shown until TDLib reports Ready. Feature-off builds stay on this copy.
pub(crate) const TDLIB_UNAVAILABLE_BANNER: &str = "TDLib unavailable in this build. Enable feature telegram-tdlib after a local TDLib install. These screens do not open a live Telegram session.";

/// Shown when TDLib is compiled but authorizationStateReady has not arrived.
pub(crate) const TELEGRAM_STUB_UNTIL_READY: &str =
    "Telegram is not authorized yet. This banner drops only after TDLib reports Ready.";

#[must_use]
pub(crate) const fn tdlib_compiled() -> bool {
    cfg!(feature = "telegram-tdlib")
}

#[must_use]
pub(crate) fn stub_banner(snapshot: &Snapshot) -> Option<&'static str> {
    if snapshot.telegram_ready() {
        return None;
    }
    if tdlib_compiled() {
        Some(TELEGRAM_STUB_UNTIL_READY)
    } else {
        Some(TDLIB_UNAVAILABLE_BANNER)
    }
}

pub(crate) fn draw(ui: &mut egui::Ui, snapshot: &mut Snapshot, secrets: &SecretStore) {
    if snapshot.auth == AuthScreen::Idle {
        return;
    }

    ui.separator();
    ui.heading(auth_heading(snapshot.auth));
    if let Some(banner) = stub_banner(snapshot) {
        ui.colored_label(WARN, banner);
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
        AuthScreen::NeedCredentials => need_credentials(ui, snapshot, secrets),
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

fn auth_heading(auth: AuthScreen) -> &'static str {
    match auth {
        AuthScreen::NeedCredentials => "Telegram API credentials are not in this build",
        AuthScreen::TelegramApi => "Advanced: custom Telegram API credentials",
        AuthScreen::TelegramPhone | AuthScreen::TelegramCode | AuthScreen::Telegram2fa => {
            "Add Telegram account"
        }
        AuthScreen::Idle => "",
    }
}

fn need_credentials(ui: &mut egui::Ui, snapshot: &mut Snapshot, secrets: &SecretStore) {
    ui.label(
        "Official / publisher binaries inject TELEGRAM_API_ID and TELEGRAM_API_HASH at compile time. They are not in the MIT git tree and are not set in public CI for fork PRs.",
    );
    ui.label(
        "Dev builds: rebuild with those env vars, or set a keychain override. Never commit sample Telegram credentials.",
    );
    ui.add_space(6.0);
    if ui
        .button("Advanced: set custom api_id / api_hash")
        .clicked()
    {
        snapshot.open_api_override(secrets);
    }
}

fn telegram_api(ui: &mut egui::Ui, snapshot: &mut Snapshot, secrets: &SecretStore) {
    ui.label(
        "Power-user override. This pair wins over a publisher inject. Help: https://my.telegram.org — API development tools.",
    );
    ui.label("Values stay in the OS keychain or memory. They are not logged.");
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
    continue_button(ui, snapshot, secrets, "Save override");
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
