//! The buttons of a bot: the inline keyboard under a message, the reply
//! keyboard over the compose field, and the answer of a bot to a press.
//!
//! A callback button sends `Intent::PressButton` with its data. A URL
//! button opens its address through the link rules. A copy button puts its
//! text on the clipboard. A reply keyboard button sends its label. A kind
//! that the app cannot do shows, but it is off, and its tooltip says why.

use eframe::egui::{self, RichText};
use thinwire_core::Intent;
use thinwire_protocol::{ButtonAction, KeyboardButton, MessageKeyboard, ProtocolId};

use super::theme::{self, radius, space};

/// Height of one button.
const BUTTON_HEIGHT: f32 = 32.0;
/// Space between two buttons, across and down.
const BUTTON_GAP: f32 = 4.0;
/// An inline keyboard is at least this wide, also under a short bubble.
const INLINE_MIN_WIDTH: f32 = 260.0;
/// How long the answer of a bot shows, when it is not an alert.
pub(crate) const NOTICE_SECS: f64 = 5.0;

/// The message and the chat that own the buttons.
pub(crate) struct Owner<'a> {
    pub protocol: ProtocolId,
    pub conversation_id: &'a str,
    pub message_id: &'a str,
    /// The data of the button that waits for its bot, in this message.
    pub pressed: Option<&'a str>,
    /// The selection mode is on: the buttons are off.
    pub selecting: bool,
}

/// The inline keyboard under a bubble. `bubble_width` is the width of the
/// bubble, and `max_width` the widest that a bubble can be.
pub(crate) fn inline(
    ui: &mut egui::Ui,
    keyboard: &MessageKeyboard,
    bubble_width: f32,
    max_width: f32,
    owner: &Owner<'_>,
    out: &mut Vec<Intent>,
) {
    let width = bubble_width
        .max(INLINE_MIN_WIDTH.min(max_width))
        .min(ui.available_width());
    ui.add_space(BUTTON_GAP);
    grid(
        ui,
        keyboard,
        width,
        ("inline-keyboard", owner.message_id),
        |ui, button, size| {
            let waiting = matches!(
                (&button.action, owner.pressed),
                (ButtonAction::Callback { data }, Some(pressed)) if data == pressed
            );
            let response = draw_button(ui, button, size, !owner.selecting, waiting);
            if !response.clicked() {
                return;
            }
            match &button.action {
                ButtonAction::Callback { data } => out.push(Intent::PressButton {
                    protocol: owner.protocol,
                    conversation_id: owner.conversation_id.to_owned(),
                    message_id: owner.message_id.to_owned(),
                    data: data.clone(),
                }),
                ButtonAction::Url { url } => super::links::open_url(url),
                ButtonAction::CopyText { text } => ui.ctx().copy_text(text.clone()),
                ButtonAction::SendText => out.push(Intent::SendKeyboardText {
                    protocol: owner.protocol,
                    conversation_id: owner.conversation_id.to_owned(),
                    text: button.text.clone(),
                }),
                ButtonAction::Unsupported => {}
            }
        },
    );
}

/// The reply keyboard of the open chat, over the compose field. A press
/// sends the label of the button.
pub(crate) fn reply(
    ui: &mut egui::Ui,
    keyboard: &MessageKeyboard,
    protocol: ProtocolId,
    conversation_id: &str,
    out: &mut Vec<Intent>,
) {
    let width = ui.available_width();
    grid(
        ui,
        keyboard,
        width,
        ("reply-keyboard", conversation_id),
        |ui, button, size| {
            let response = draw_button(ui, button, size, true, false);
            if response.clicked() && button.action == ButtonAction::SendText {
                out.push(Intent::SendKeyboardText {
                    protocol,
                    conversation_id: conversation_id.to_owned(),
                    text: button.text.clone(),
                });
            }
        },
    );
    ui.add_space(space::XS);
}

/// The rows of a keyboard. The buttons of one row share its width.
fn grid(
    ui: &mut egui::Ui,
    keyboard: &MessageKeyboard,
    width: f32,
    salt: impl std::hash::Hash + std::fmt::Debug,
    mut each: impl FnMut(&mut egui::Ui, &KeyboardButton, egui::Vec2),
) {
    ui.push_id(salt, |ui| {
        ui.allocate_ui_with_layout(
            egui::vec2(width, 0.0),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(BUTTON_GAP, BUTTON_GAP);
                // A click target under the buttons. A click on the gap
                // between two buttons, or on a button that is off, stops
                // here: it does not reach the line of the message, so a
                // fast second click does not start a reply.
                let rows = keyboard.rows.iter().filter(|row| !row.is_empty()).count() as f32;
                let height = rows * BUTTON_HEIGHT + (rows - 1.0).max(0.0) * BUTTON_GAP;
                let under = egui::Rect::from_min_size(ui.cursor().min, egui::vec2(width, height));
                ui.interact(under, ui.scope_id().with("under"), egui::Sense::CLICK);
                for (row_index, row) in keyboard.rows.iter().enumerate() {
                    if row.is_empty() {
                        continue;
                    }
                    let count = row.len() as f32;
                    let button_width = ((width - BUTTON_GAP * (count - 1.0)) / count).max(1.0);
                    ui.push_id(row_index, |ui| {
                        ui.horizontal(|ui| {
                            for (column, button) in row.iter().enumerate() {
                                ui.push_id(column, |ui| {
                                    each(ui, button, egui::vec2(button_width, BUTTON_HEIGHT));
                                });
                            }
                        });
                    });
                }
            },
        );
    });
}

fn draw_button(
    ui: &mut egui::Ui,
    button: &KeyboardButton,
    size: egui::Vec2,
    enabled: bool,
    waiting: bool,
) -> egui::Response {
    let palette = theme::palette(ui);
    let supported = button.action != ButtonAction::Unsupported;
    let mut label = button.text.clone();
    if let ButtonAction::Url { .. } = button.action {
        // An arrow says that the button opens the browser.
        label.push_str(" \u{2197}");
    }
    let ink = if supported {
        palette.text
    } else {
        palette.text3
    };
    let widget = egui::Button::new(RichText::new(label).color(ink))
        .truncate()
        .min_size(size)
        .fill(palette.surface)
        .stroke(egui::Stroke::new(1.0, palette.border))
        .corner_radius(egui::CornerRadius::same(radius::CONTROL));
    let response = ui.add_enabled_ui(enabled && supported && !waiting, |ui| {
        ui.add_sized(size, widget)
    });
    let mut response = response.inner;
    if waiting {
        let center = response.rect.right_center() - egui::vec2(size.y / 2.0, 0.0);
        super::motion::spinner_at(
            ui,
            egui::Rect::from_center_size(center, egui::Vec2::splat(size.y * 0.45)),
        );
    }
    let hover = match &button.action {
        ButtonAction::Url { url } => Some(url.clone()),
        ButtonAction::CopyText { .. } => Some("Copy to clipboard".to_owned()),
        ButtonAction::Unsupported => Some("Open this button in the Telegram app.".to_owned()),
        ButtonAction::Callback { .. } | ButtonAction::SendText => None,
    };
    if let Some(hover) = hover {
        response = response
            .on_hover_text(hover)
            .on_disabled_hover_text(if supported {
                ""
            } else {
                "Open this button in the Telegram app."
            });
    }
    response
}

/// The answer of a bot to a press, over the bottom of the thread. A plain
/// answer goes away after [`NOTICE_SECS`]. An alert stays until OK.
pub(crate) fn notice(
    ui: &mut egui::Ui,
    notice: &thinwire_core::ButtonNotice,
    out: &mut Vec<Intent>,
) {
    let now = ui.input(|input| input.time);
    let since_id = egui::Id::unique(("bot-notice-since", notice.serial));
    let since = ui
        .ctx()
        .data_mut(|data| *data.get_temp_mut_or(since_id, now));
    if !notice.alert {
        let left = NOTICE_SECS - (now - since);
        if left <= 0.0 {
            out.push(Intent::CloseButtonNotice);
            return;
        }
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_secs_f64(left));
    }
    let palette = theme::palette(ui);
    egui::Frame::new()
        .fill(palette.surface)
        .stroke(egui::Stroke::new(1.0, palette.border_strong))
        .corner_radius(egui::CornerRadius::same(radius::CARD))
        .inner_margin(egui::Margin::symmetric(space::M as i8, space::S as i8))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let text = ui
                    .add(egui::Label::new(RichText::new(&notice.text).color(palette.text)).wrap());
                text.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::Role::Label, true, notice.text.as_str())
                });
                let close = if notice.alert { "OK" } else { "Close" };
                if ui.button(close).clicked() {
                    out.push(Intent::CloseButtonNotice);
                }
            });
        });
}

/// The part over the compose field: the answer of a bot for the open chat,
/// then the reply keyboard of that chat.
pub(crate) fn bar(ui: &mut egui::Ui, snapshot: &thinwire_core::View<'_>, out: &mut Vec<Intent>) {
    let top = ui.cursor().top();
    let conversation = snapshot.pane.conversation.as_deref();
    if let Some(shown) = snapshot.button_notice().filter(|shown| {
        shown.protocol == snapshot.pane.protocol
            && Some(shown.conversation_id.as_str()) == conversation
    }) {
        notice(ui, shown, out);
        ui.add_space(space::XS);
    }
    if let (Some(keyboard), Some(conversation_id)) = (snapshot.reply_keyboard(), conversation) {
        reply(
            ui,
            &keyboard.keyboard,
            snapshot.pane.protocol,
            conversation_id,
            out,
        );
    }
    let height = ui.cursor().top() - top;
    let id = bar_height_id();
    let before = ui
        .ctx()
        .data(|data| data.get_temp::<f32>(id))
        .unwrap_or(0.0);
    if (height - before).abs() > 0.5 {
        ui.ctx().data_mut(|data| data.insert_temp(id, height));
        // The thread above uses this height: draw it again with the new one.
        ui.ctx().request_repaint();
    }
}

/// The height of [`bar`] in the last frame, for the space that the thread
/// leaves to the compose bar.
pub(crate) fn bar_height(ui: &egui::Ui) -> f32 {
    ui.ctx()
        .data(|data| data.get_temp::<f32>(bar_height_id()))
        .unwrap_or(0.0)
}

fn bar_height_id() -> egui::Id {
    egui::Id::unique("thinwire-bot-bar-height")
}
