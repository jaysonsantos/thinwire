//! Non-modal Telegram login. Cancel is always available. Secrets stay off disk
//! except in the OS keychain (or in-memory when the keychain is unavailable).

use eframe::egui::{self, RichText};

use thinwire_protocol::{ProtocolId, TelegramAuthError, TelegramCodeVia};

use super::motion;
use super::theme::{self, space};
use super::ui::edited;
pub(crate) use thinwire_core::state::stub_banner;
use thinwire_core::state::{AuthScreen, Snapshot};
use thinwire_core::{AuthField, Intent, SecretText, TelegramIntent, View};

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

/// Place of the step among the three login steps. Other screens have none.
#[must_use]
fn step_number(auth: AuthScreen) -> Option<usize> {
    match auth {
        AuthScreen::TelegramPhone => Some(1),
        AuthScreen::TelegramCode => Some(2),
        AuthScreen::Telegram2fa => Some(3),
        AuthScreen::Idle
        | AuthScreen::NeedCredentials
        | AuthScreen::TelegramApi
        | AuthScreen::TelegramConnecting => None,
    }
}

/// Three segments, one for each login step, then the step caption. The
/// segments up to the current step are filled with the accent.
fn step_progress(ui: &mut egui::Ui, auth: AuthScreen) {
    let (Some(current), Some(caption)) = (step_number(auth), step_line(auth)) else {
        return;
    };
    let palette = theme::palette(ui);
    let card = if ui.visuals().dark_mode {
        palette.surface
    } else {
        palette.bg
    };
    let track = motion::mix(card, palette.text3, 0.35);
    ui.add_space(space::S);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = space::XS;
        for step in 1..=3 {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(22.0, 4.0), egui::Sense::hover());
            let color = if step <= current {
                palette.accent
            } else {
                track
            };
            ui.painter()
                .rect_filled(rect, egui::CornerRadius::same(2), color);
        }
        ui.add_space(space::XS);
        ui.label(RichText::new(caption).small().color(palette.text3));
    });
}

pub(crate) fn draw(ui: &mut egui::Ui, snapshot: &View<'_>, out: &mut Vec<Intent>) {
    if snapshot.auth == AuthScreen::Idle {
        return;
    }
    // The card sits in the middle of the column. Its height is known from the
    // last frame, so the first frame of a step does not jump.
    let height_id = ui.scope_id().with("auth-card-height");
    let known = ui.data(|data| data.get_temp::<f32>(height_id).unwrap_or(0.0));
    ui.add_space(((ui.available_height() - known) * 0.4).max(0.0));
    let card = ui
        .scope(|ui| {
            theme::show_centered_card(ui, |ui| card_body(ui, snapshot, out));
        })
        .response;
    ui.data_mut(|data| data.insert_temp(height_id, card.rect.height()));
}

fn card_body(ui: &mut egui::Ui, snapshot: &View<'_>, out: &mut Vec<Intent>) {
    let palette = theme::palette(ui);
    let (tile, _) = ui.allocate_exact_size(egui::vec2(44.0, 44.0), egui::Sense::hover());
    super::ui::paint_protocol_tile(ui, tile, ProtocolId::Telegram, None);
    ui.add_space(space::M);
    ui.label(
        RichText::new(auth_heading(snapshot.auth))
            .text_style(theme::display())
            .color(palette.text),
    );
    step_progress(ui, snapshot.auth);
    if let Some(banner) = stub_banner(snapshot) {
        ui.add_space(space::M);
        super::ui::note_box(ui, palette.warn, banner);
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
        AuthScreen::NeedCredentials => need_credentials(ui, out),
        AuthScreen::TelegramApi => telegram_api(ui, snapshot, focus, out),
        AuthScreen::TelegramConnecting => telegram_connecting(ui, snapshot, out),
        AuthScreen::TelegramPhone => telegram_phone(ui, snapshot, focus, out),
        AuthScreen::TelegramCode => telegram_code(ui, snapshot, focus, out),
        AuthScreen::Telegram2fa => telegram_2fa(ui, snapshot, focus, out),
    }
    inline_feedback(ui, snapshot);
    ui.add_space(space::S);
    if ui
        .add(egui::Button::new(RichText::new("Cancel").color(palette.text2)).frame(false))
        .clicked()
    {
        out.push(Intent::Telegram(TelegramIntent::Cancel));
    }
}

/// Notice and error under the field. The status strip does not repeat them.
fn inline_feedback(ui: &mut egui::Ui, snapshot: &Snapshot) {
    let palette = theme::palette(ui);
    if snapshot.auth_notice.is_none() && snapshot.error.is_none() {
        return;
    }
    ui.add_space(space::S);
    if let Some(notice) = &snapshot.auth_notice {
        super::ui::note_box(ui, palette.warn, notice);
    }
    if let Some(error) = &snapshot.error {
        egui::Frame::new()
            .fill(super::motion::mix(palette.bg, palette.error, 0.12))
            .corner_radius(egui::CornerRadius::same(theme::radius::ROW))
            .inner_margin(egui::Margin::symmetric(space::M as i8, space::S as i8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    RichText::new(&error.happened)
                        .text_style(theme::secondary())
                        .family(theme::semibold())
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
            });
    }
}

/// Focus the step field when the step opens, and again when a submit ends.
fn take_focus(ui: &egui::Ui, snapshot: &Snapshot) -> bool {
    let key = egui::Id::unique("auth-last-step");
    let now = (snapshot.auth, snapshot.auth_busy);
    let previous = ui.data_mut(|data| {
        let previous = data.get_temp::<(AuthScreen, bool)>(key);
        data.insert_temp(key, now);
        previous
    });
    previous != Some(now) && !snapshot.auth_busy
}

/// One login field. Returns the new text when the user changed it.
fn field(
    ui: &mut egui::Ui,
    current: &str,
    auth_field: AuthField,
    focus: bool,
    edit: impl FnOnce(&mut String) -> egui::TextEdit<'_>,
) -> Option<String> {
    let id = egui::Id::unique(("auth-field", auth_field));
    let palette = theme::palette(ui);
    let margin = egui::Margin::symmetric(space::M as i8, space::M as i8);
    let focused = ui.memory(|memory| memory.has_focus(id));
    let ring = motion::toggle(ui.ctx(), id.with("ring"), focused, motion::STATE);
    let frame = theme::field_frame(palette, ring, margin);
    let width = ui.available_width();
    let changed = edited(ui, current, |text| {
        edit(text).id(id).frame(frame).desired_width(width)
    });
    if focus {
        ui.memory_mut(|memory| memory.request_focus(id));
    }
    changed
}

/// Send a login field value to the core as a [`SecretText`].
fn set_field(out: &mut Vec<Intent>, auth_field: AuthField, value: String) {
    out.push(Intent::Telegram(TelegramIntent::SetField(
        auth_field,
        SecretText::new(value),
    )));
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

fn need_credentials(ui: &mut egui::Ui, out: &mut Vec<Intent>) {
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
        out.push(Intent::Telegram(TelegramIntent::OpenApiOverride));
    }
}

fn telegram_api(ui: &mut egui::Ui, snapshot: &Snapshot, focus: bool, out: &mut Vec<Intent>) {
    ui.label(
        "Power-user override. This pair wins over a publisher inject. Help: https://my.telegram.org — API development tools.",
    );
    ui.label("Values stay in the OS keychain or memory. They are not logged.");
    ui.horizontal(|ui| {
        ui.label("api_id");
        if let Some(value) = field(
            ui,
            &snapshot.telegram_api_id,
            AuthField::ApiId,
            focus,
            |text| egui::TextEdit::singleline(text).hint_text("numeric id from my.telegram.org"),
        ) {
            set_field(out, AuthField::ApiId, value);
        }
    });
    ui.horizontal(|ui| {
        ui.label("api_hash");
        if let Some(value) = field(
            ui,
            &snapshot.telegram_api_hash,
            AuthField::ApiHash,
            false,
            |text| {
                egui::TextEdit::singleline(text)
                    .password(true)
                    .hint_text("from my.telegram.org")
            },
        ) {
            set_field(out, AuthField::ApiHash, value);
        }
    });
    continue_button(ui, snapshot, "Save override", out);
}

/// Between Add Telegram and the phone step.
///
/// The first connect is busy until Telegram asks for the phone. The shared
/// continue control then shows a spinner and "Connecting to Telegram…".
/// After a failed start the same control shows Try again.
fn telegram_connecting(ui: &mut egui::Ui, snapshot: &Snapshot, out: &mut Vec<Intent>) {
    continue_button(ui, snapshot, "Try again", out);
}

fn telegram_phone(ui: &mut egui::Ui, snapshot: &Snapshot, focus: bool, out: &mut Vec<Intent>) {
    field_label(ui, "Your phone number, with + and the country code.");
    if let Some(value) = field(
        ui,
        &snapshot.telegram_phone,
        AuthField::Phone,
        focus,
        |text| egui::TextEdit::singleline(text).hint_text("+15551234567"),
    ) {
        set_field(out, AuthField::Phone, value);
    }
    continue_button(ui, snapshot, "Send code", out);
}

fn telegram_code(ui: &mut egui::Ui, snapshot: &Snapshot, focus: bool, out: &mut Vec<Intent>) {
    field_label(ui, code_hint(snapshot.code_via));
    let digits_only = code_is_digits(snapshot.code_via);
    let hint = if digits_only {
        "12345"
    } else {
        "word or phrase"
    };
    if let Some(mut value) = field(
        ui,
        &snapshot.telegram_code,
        AuthField::Code,
        focus,
        |text| egui::TextEdit::singleline(text).hint_text(hint),
    ) {
        if digits_only {
            value.retain(|c| c.is_ascii_digit());
        }
        set_field(out, AuthField::Code, value);
    }
    continue_button(ui, snapshot, "Continue", out);
    ui.horizontal(|ui| {
        if ui.link("Change number").clicked() {
            out.push(Intent::Telegram(TelegramIntent::ChangeNumber));
        }
        if snapshot.auth_rejection == Some(TelegramAuthError::CodeExpired)
            && ui.link("Send a new code").clicked()
        {
            out.push(Intent::Telegram(TelegramIntent::ResendCode));
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

fn telegram_2fa(ui: &mut egui::Ui, snapshot: &Snapshot, focus: bool, out: &mut Vec<Intent>) {
    field_label(ui, "Enter your Telegram password.");
    if let Some(value) = field(
        ui,
        &snapshot.telegram_2fa,
        AuthField::Password,
        focus,
        |text| {
            egui::TextEdit::singleline(text)
                .password(true)
                .hint_text("password")
        },
    ) {
        set_field(out, AuthField::Password, value);
    }
    continue_button(ui, snapshot, "Finish", out);
}

fn continue_button(ui: &mut egui::Ui, snapshot: &Snapshot, label: &str, out: &mut Vec<Intent>) {
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
                    motion::spinner(ui);
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
        out.push(Intent::Telegram(TelegramIntent::Submit));
    }
}

#[cfg(test)]
mod tests {
    use super::{AuthScreen, step_line};

    #[test]
    fn the_progress_segments_follow_the_three_login_steps() {
        use super::step_number;

        assert_eq!(step_number(AuthScreen::TelegramPhone), Some(1));
        assert_eq!(step_number(AuthScreen::TelegramCode), Some(2));
        assert_eq!(step_number(AuthScreen::Telegram2fa), Some(3));
        assert_eq!(step_number(AuthScreen::TelegramConnecting), None);
        assert_eq!(step_number(AuthScreen::Idle), None);
    }

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
            connecting.contains("continue_button(ui, snapshot, \"Try again\", out)"),
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
        let spinner = button.find("motion::spinner(").expect("spinner");
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
