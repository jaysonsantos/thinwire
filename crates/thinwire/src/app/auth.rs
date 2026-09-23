//! Non-modal Telegram login. Cancel is always available. Secrets stay off disk
//! except in the OS keychain (or in-memory when the keychain is unavailable).

use eframe::egui::{self, Color32, RichText};

use thinwire_protocol::{TelegramAuthError, TelegramCodeVia};

use super::secrets::SecretStore;
use super::snapshot::{AuthScreen, Snapshot};

const MUTED: Color32 = Color32::from_rgb(160, 160, 168);
const WARN: Color32 = Color32::from_rgb(214, 160, 64);

/// Shown until TDLib reports Ready. Feature-off builds stay on this copy.
pub(crate) const TDLIB_UNAVAILABLE_BANNER: &str = "TDLib unavailable in this build. Enable feature telegram-tdlib after a local TDLib install. These screens do not open a live Telegram session.";

/// Shown when TDLib is compiled but authorizationStateReady has not arrived.
/// It drops only on Ready (ADR 0006). End-user copy: no library names.
pub(crate) const TELEGRAM_STUB_UNTIL_READY: &str = "Telegram is not signed in yet.";

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
        RichText::new("Your number and code stay on this device. They are never logged.")
            .small()
            .color(MUTED),
    );
    if snapshot.auth_busy {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(RichText::new("Waiting for Telegram…").small().color(MUTED));
        });
    }

    if let Some(notice) = snapshot.auth_notice {
        ui.colored_label(WARN, notice);
    }

    let focus = take_focus(ui, snapshot);
    match snapshot.auth {
        AuthScreen::Idle => {}
        AuthScreen::NeedCredentials => need_credentials(ui, snapshot, secrets),
        AuthScreen::TelegramApi => telegram_api(ui, snapshot, secrets, focus),
        AuthScreen::TelegramConnecting => telegram_connecting(ui, snapshot, secrets),
        AuthScreen::TelegramPhone => telegram_phone(ui, snapshot, secrets, focus),
        AuthScreen::TelegramCode => telegram_code(ui, snapshot, secrets, focus),
        AuthScreen::Telegram2fa => telegram_2fa(ui, snapshot, secrets, focus),
    }

    ui.add_space(8.0);
    if ui.button("Cancel").clicked() {
        snapshot.cancel_auth(secrets);
    }
}

/// Focus the step field when the step opens, and again when a submit ends.
fn take_focus(ui: &egui::Ui, snapshot: &Snapshot) -> bool {
    let key = egui::Id::new("auth-last-step");
    let now = (snapshot.auth, snapshot.auth_busy);
    let previous = ui.data_mut(|data| {
        let previous = data.get_temp::<(AuthScreen, bool)>(key);
        data.insert_temp(key, now);
        previous
    });
    previous != Some(now) && !snapshot.auth_busy
}

fn field(ui: &mut egui::Ui, edit: egui::TextEdit<'_>, id: &str, focus: bool) {
    let id = egui::Id::new(("auth-field", id));
    ui.add(edit.id(id));
    if focus {
        ui.memory_mut(|memory| memory.request_focus(id));
    }
}

fn auth_heading(auth: AuthScreen) -> &'static str {
    match auth {
        AuthScreen::NeedCredentials => "Telegram API credentials missing",
        AuthScreen::TelegramApi => "Advanced: custom Telegram API credentials",
        AuthScreen::TelegramConnecting | AuthScreen::TelegramPhone | AuthScreen::TelegramCode => {
            "Add Telegram account"
        }
        AuthScreen::Telegram2fa => "Two-step verification",
        AuthScreen::Idle => "",
    }
}

fn need_credentials(ui: &mut egui::Ui, snapshot: &mut Snapshot, secrets: &SecretStore) {
    ui.label(
        "Credentials missing. Official / publisher binaries inject TELEGRAM_API_ID and TELEGRAM_API_HASH at compile time. They are not in the MIT git tree and are not set in public CI for fork PRs.",
    );
    ui.label(
        "Dev / unofficial builds: rebuild with those env vars, or set a keychain override in Advanced. Never commit sample Telegram credentials.",
    );
    ui.add_space(6.0);
    if ui
        .button("Advanced: set custom api_id / api_hash")
        .clicked()
    {
        snapshot.open_api_override(secrets);
    }
}

fn telegram_api(ui: &mut egui::Ui, snapshot: &mut Snapshot, secrets: &SecretStore, focus: bool) {
    ui.label(
        "Power-user override. This pair wins over a publisher inject. Help: https://my.telegram.org — API development tools.",
    );
    ui.label("Values stay in the OS keychain or memory. They are not logged.");
    ui.horizontal(|ui| {
        ui.label("api_id");
        field(
            ui,
            egui::TextEdit::singleline(&mut snapshot.telegram_api_id)
                .hint_text("numeric id from my.telegram.org"),
            "api_id",
            focus,
        );
    });
    ui.horizontal(|ui| {
        ui.label("api_hash");
        field(
            ui,
            egui::TextEdit::singleline(&mut snapshot.telegram_api_hash)
                .password(true)
                .hint_text("from my.telegram.org"),
            "api_hash",
            false,
        );
    });
    continue_button(ui, snapshot, secrets, "Save override");
}

/// Between Add Telegram and the phone step. Busy shows the spinner above;
/// after a failed start, the error block explains and Try again retries.
fn telegram_connecting(ui: &mut egui::Ui, snapshot: &mut Snapshot, secrets: &SecretStore) {
    if !snapshot.auth_busy {
        continue_button(ui, snapshot, secrets, "Try again");
    }
}

fn telegram_phone(ui: &mut egui::Ui, snapshot: &mut Snapshot, secrets: &SecretStore, focus: bool) {
    ui.label("Your phone number, with + and the country code.");
    field(
        ui,
        egui::TextEdit::singleline(&mut snapshot.telegram_phone).hint_text("+15551234567"),
        "phone",
        focus,
    );
    continue_button(ui, snapshot, secrets, "Send code");
}

fn telegram_code(ui: &mut egui::Ui, snapshot: &mut Snapshot, secrets: &SecretStore, focus: bool) {
    ui.label(code_hint(snapshot.code_via));
    let digits_only = code_is_digits(snapshot.code_via);
    let hint = if digits_only {
        "12345"
    } else {
        "word or phrase"
    };
    field(
        ui,
        egui::TextEdit::singleline(&mut snapshot.telegram_code).hint_text(hint),
        "code",
        focus,
    );
    if digits_only {
        snapshot.telegram_code.retain(|c| c.is_ascii_digit());
    }
    continue_button(ui, snapshot, secrets, "Continue");
    ui.horizontal(|ui| {
        if ui.link("Change number").clicked() {
            snapshot.change_number();
        }
        if snapshot.auth_rejection == Some(TelegramAuthError::CodeExpired)
            && ui.link("Send a new code").clicked()
        {
            snapshot.resend_code(secrets);
        }
    });
}

/// Digits only, unless Telegram sent a word or phrase (qa R14).
#[must_use]
pub(crate) fn code_is_digits(via: Option<TelegramCodeVia>) -> bool {
    via.is_none_or(TelegramCodeVia::digits_only)
}

fn code_hint(via: Option<TelegramCodeVia>) -> &'static str {
    match via {
        Some(TelegramCodeVia::TelegramApp) => {
            "Telegram sent the code to your Telegram app on another device."
        }
        Some(TelegramCodeVia::Sms) => "Telegram sent the code by SMS.",
        Some(TelegramCodeVia::SmsWord) => "Telegram sent a word or a phrase by SMS. Type it here.",
        Some(TelegramCodeVia::Call) => "Telegram will call you with the code.",
        Some(TelegramCodeVia::Other) | None => "Enter the code Telegram sent you.",
    }
}

fn telegram_2fa(ui: &mut egui::Ui, snapshot: &mut Snapshot, secrets: &SecretStore, focus: bool) {
    ui.label("Enter your Telegram password.");
    field(
        ui,
        egui::TextEdit::singleline(&mut snapshot.telegram_2fa)
            .password(true)
            .hint_text("password"),
        "password",
        focus,
    );
    continue_button(ui, snapshot, secrets, "Finish");
}

fn continue_button(ui: &mut egui::Ui, snapshot: &mut Snapshot, secrets: &SecretStore, label: &str) {
    ui.add_enabled_ui(snapshot.can_submit_auth(), |ui| {
        if ui.button(label).clicked() {
            snapshot.advance_telegram(secrets);
        }
    });
}
