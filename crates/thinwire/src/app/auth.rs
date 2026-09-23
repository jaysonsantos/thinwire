//! Non-modal Telegram login. Cancel is always available. Secrets stay off disk
//! except in the OS keychain (or in-memory when the keychain is unavailable).

use eframe::egui::{self, RichText};

use thinwire_protocol::{TelegramAuthError, TelegramCodeVia};

use super::snapshot::{AuthScreen, Snapshot};
use super::theme::{self, space};
use thinwire_core::secrets::SecretStore;

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

/// Step caption for the three login fields. Other screens have none.
#[must_use]
fn step_line(auth: AuthScreen) -> Option<&'static str> {
    match auth {
        AuthScreen::TelegramPhone => Some("Step 1 of 3 · Phone"),
        AuthScreen::TelegramCode => Some("Step 2 of 3 · Code"),
        AuthScreen::Telegram2fa => Some("Step 3 of 3 · Password"),
        AuthScreen::Idle
        | AuthScreen::NeedCredentials
        | AuthScreen::TelegramApi
        | AuthScreen::TelegramConnecting => None,
    }
}

pub(crate) fn draw(ui: &mut egui::Ui, snapshot: &mut Snapshot, secrets: &SecretStore) {
    if snapshot.auth == AuthScreen::Idle {
        return;
    }
    theme::show_centered_card(ui, |ui| {
        let palette = theme::palette(ui);
        ui.label(
            RichText::new(auth_heading(snapshot.auth))
                .text_style(theme::display())
                .color(palette.text),
        );
        if let Some(step) = step_line(snapshot.auth) {
            ui.add_space(space::XS);
            ui.label(RichText::new(step).small().color(palette.text3));
        }
        if let Some(banner) = stub_banner(snapshot) {
            ui.add_space(space::S);
            ui.colored_label(palette.warn, banner);
        }
        ui.add_space(space::S);
        ui.label(
            RichText::new("Your number and code stay on this device. They are never logged.")
                .small()
                .color(palette.text3),
        );
        ui.add_space(space::M);
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
        inline_feedback(ui, snapshot);
        ui.add_space(space::S);
        if ui
            .add(egui::Button::new(RichText::new("Cancel").color(palette.text2)).frame(false))
            .clicked()
        {
            snapshot.cancel_auth(secrets);
        }
    });
}

/// Notice and error under the field. The status strip does not repeat them.
fn inline_feedback(ui: &mut egui::Ui, snapshot: &Snapshot) {
    let palette = theme::palette(ui);
    if snapshot.auth_notice.is_none() && snapshot.error.is_none() {
        return;
    }
    ui.add_space(space::S);
    if let Some(notice) = &snapshot.auth_notice {
        ui.label(
            RichText::new(notice)
                .text_style(theme::secondary())
                .color(palette.warn),
        );
    }
    if let Some(error) = &snapshot.error {
        ui.label(
            RichText::new(&error.happened)
                .text_style(theme::secondary())
                .color(palette.error),
        );
        ui.label(
            RichText::new(&error.why)
                .text_style(theme::secondary())
                .color(palette.text2),
        );
        ui.label(
            RichText::new(format!("What to do: {}", error.next))
                .text_style(theme::secondary())
                .color(palette.text2),
        );
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
    ui.add(
        edit.id(id)
            .desired_width(ui.available_width())
            .margin(egui::Margin::symmetric(space::M as i8, space::M as i8)),
    );
    if focus {
        ui.memory_mut(|memory| memory.request_focus(id));
    }
}

fn field_label(ui: &mut egui::Ui, text: &str) {
    ui.label(
        RichText::new(text)
            .text_style(theme::secondary())
            .color(theme::palette(ui).text2),
    );
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

/// Between Add Telegram and the phone step.
///
/// The first connect is busy until Telegram asks for the phone. The shared
/// continue control then shows a spinner and "Connecting to Telegram…".
/// After a failed start the same control shows Try again.
fn telegram_connecting(ui: &mut egui::Ui, snapshot: &mut Snapshot, secrets: &SecretStore) {
    continue_button(ui, snapshot, secrets, "Try again");
}

fn telegram_phone(ui: &mut egui::Ui, snapshot: &mut Snapshot, secrets: &SecretStore, focus: bool) {
    field_label(ui, "Your phone number, with + and the country code.");
    field(
        ui,
        egui::TextEdit::singleline(&mut snapshot.telegram_phone).hint_text("+15551234567"),
        "phone",
        focus,
    );
    continue_button(ui, snapshot, secrets, "Send code");
}

fn telegram_code(ui: &mut egui::Ui, snapshot: &mut Snapshot, secrets: &SecretStore, focus: bool) {
    field_label(ui, code_hint(snapshot.code_via));
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
            snapshot.resend_code();
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
    field_label(ui, "Enter your Telegram password.");
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
    let palette = theme::palette(ui);
    if snapshot.auth_busy {
        ui.add_space(space::S);
        egui::Frame::new()
            .fill(palette.surface)
            .corner_radius(egui::CornerRadius::same(theme::radius::CONTROL))
            .inner_margin(egui::Margin::symmetric(space::M as i8, space::S as i8))
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.set_min_height(40.0);
                let text = if snapshot.auth == AuthScreen::TelegramConnecting {
                    "Connecting to Telegram…"
                } else {
                    "Waiting for Telegram…"
                };
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(
                        RichText::new(text)
                            .text_style(theme::secondary())
                            .color(palette.text2),
                    );
                });
            });
        return;
    }
    ui.add_space(space::S);
    let can_submit = snapshot.can_submit_auth();
    let (fill, label_color) = if can_submit {
        (palette.accent, palette.on_accent)
    } else {
        (palette.surface, palette.text3)
    };
    if ui
        .add_enabled(
            can_submit,
            egui::Button::new(RichText::new(label).color(label_color))
                .fill(fill)
                .min_size(egui::vec2(ui.available_width(), 40.0)),
        )
        .clicked()
    {
        snapshot.advance_telegram(secrets);
    }
}

#[cfg(test)]
mod tests {
    use super::{AuthScreen, step_line};

    #[test]
    fn step_line_names_the_three_login_steps() {
        assert_eq!(
            step_line(AuthScreen::TelegramPhone),
            Some("Step 1 of 3 · Phone")
        );
        assert_eq!(
            step_line(AuthScreen::TelegramCode),
            Some("Step 2 of 3 · Code")
        );
        assert_eq!(
            step_line(AuthScreen::Telegram2fa),
            Some("Step 3 of 3 · Password")
        );
        assert_eq!(step_line(AuthScreen::TelegramConnecting), None);
        assert_eq!(step_line(AuthScreen::Idle), None);
    }

    #[test]
    fn connecting_shows_waiting_chrome_while_busy() {
        let auth = include_str!("auth.rs");
        let connecting = &auth[auth.find("fn telegram_connecting(").expect("connecting")..];
        let connecting = &connecting[..connecting.find("\nfn ").expect("next")];
        assert!(
            connecting.contains("continue_button(ui, snapshot, secrets, \"Try again\")"),
            "Try again stays on the connecting screen"
        );
        assert!(
            !connecting.contains("auth_busy"),
            "a busy connection still reaches the shared continue control"
        );
        let button = &auth[auth.find("fn continue_button(").expect("button")..];
        let button = &button[..button.find("\n#[cfg(test)]").expect("tests")];
        let busy = button.find("if snapshot.auth_busy").expect("busy branch");
        let connecting = button
            .find("Connecting to Telegram…")
            .expect("connecting text");
        let waiting = button.find("Waiting for Telegram…").expect("waiting text");
        let spinner = button.find("ui.spinner()").expect("spinner");
        let early_return = button.find("return;").expect("busy returns");
        let label = button.find("RichText::new(label)").expect("idle label");
        assert!(busy < connecting && connecting < waiting);
        assert!(waiting < spinner && spinner < early_return);
        assert!(button.contains("AuthScreen::TelegramConnecting"));
        assert!(
            early_return < label,
            "Try again is the label only when the login is not busy"
        );
    }
}
