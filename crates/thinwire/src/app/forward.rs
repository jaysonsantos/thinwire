//! Forward of one or more messages (#512): the selection bar over the
//! compose field, and the chat picker. The core keeps the selection and the
//! picker. This module draws them and sends intents.

use eframe::egui::{self, RichText};
use thinwire_core::{Intent, SecretText, View};

use super::theme::{self, space};

/// The width of the forward picker.
const PICKER_WIDTH: f32 = 340.0;

/// The tallest list of chats in the picker before it scrolls.
const PICKER_LIST_HEIGHT: f32 = 320.0;

fn picker_id() -> egui::Id {
    egui::Id::unique("thinwire-forward-picker")
}

/// "1 selected", "3 selected".
fn selected_text(count: usize) -> String {
    format!("{count} selected")
}

/// The title of the picker.
fn picker_title(count: usize) -> String {
    if count == 1 {
        "Forward message".to_owned()
    } else {
        format!("Forward {count} messages")
    }
}

/// The bar over the compose field while messages of the open thread are
/// selected: the count, Forward, and Cancel.
pub(crate) fn selection_bar(ui: &mut egui::Ui, snapshot: &View<'_>, out: &mut Vec<Intent>) {
    let count = snapshot.selected_for_forward().len();
    if count == 0 {
        return;
    }
    let Some(conversation_id) = snapshot.selected_conversation.clone() else {
        return;
    };
    let palette = theme::palette(ui);
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(selected_text(count))
                .small()
                .family(theme::semibold())
                .color(palette.accent),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .button("Cancel")
                .on_hover_text("End the selection")
                .clicked()
            {
                out.push(Intent::ClearSelection);
            }
            if ui.button("Forward…").clicked() {
                out.push(Intent::OpenForward {
                    protocol: snapshot.selected_protocol,
                    conversation_id,
                    message_id: None,
                });
            }
        });
    });
    ui.add_space(space::XS);
}

/// The height of [`selection_bar`]: nothing when no message is selected.
pub(crate) fn selection_bar_height(ui: &egui::Ui, snapshot: &View<'_>) -> f32 {
    if snapshot.selected_for_forward().is_empty() {
        return 0.0;
    }
    ui.spacing().interact_size.y + ui.spacing().item_spacing.y + space::XS
}

/// Close the picker, or end the selection, as Esc does. `true` when one of
/// them was open.
pub(crate) fn take_escape(snapshot: &View<'_>, out: &mut Vec<Intent>) -> bool {
    if snapshot.forward_picker().is_some() {
        out.push(Intent::CancelForward);
        return true;
    }
    if snapshot.selecting() {
        out.push(Intent::ClearSelection);
        return true;
    }
    false
}

/// The chat picker over the window, while it is open. A click on a chat
/// forwards to it.
pub(crate) fn picker(ctx: &egui::Context, snapshot: &View<'_>, out: &mut Vec<Intent>) {
    let Some(picker) = snapshot.forward_picker() else {
        return;
    };
    let mut chosen = None;
    let mut cancel = false;
    let modal = egui::Modal::new(picker_id()).show(ctx, |ui| {
        let palette = theme::palette(ui);
        ui.set_width(PICKER_WIDTH);
        ui.label(RichText::new(picker_title(picker.count)).strong());
        ui.add_space(space::S);
        let mut query = picker.query.to_owned();
        let field = ui.add(
            egui::TextEdit::singleline(&mut query)
                .id(picker_id().with("search"))
                .hint_text("Search chats")
                .desired_width(f32::INFINITY),
        );
        super::a11y::name(ui.ctx(), field.id, "Search chats");
        if ui.memory(|memory| memory.focused().is_none()) {
            field.request_focus();
        }
        if field.changed() {
            out.push(Intent::SetForwardQuery(SecretText::new(query)));
        }
        ui.add_space(space::S);
        egui::ScrollArea::vertical()
            .max_height(PICKER_LIST_HEIGHT)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                if picker.targets.is_empty() {
                    ui.label(RichText::new("No chat matches.").color(palette.text2));
                }
                for row in &picker.targets {
                    let button = egui::Button::new(&row.title)
                        .frame(false)
                        .truncate()
                        .min_size(egui::vec2(ui.available_width(), 0.0));
                    let response = ui.add(button);
                    // The inbox row has the same title: name what this one does.
                    response.widget_info(|| {
                        egui::WidgetInfo::labeled(
                            egui::Role::Button,
                            true,
                            format!("Forward to {}", row.title),
                        )
                    });
                    if response.clicked() {
                        chosen = Some(row.id.clone());
                    }
                }
            });
        ui.add_space(space::M);
        if ui.button("Cancel").clicked() {
            cancel = true;
        }
    });
    if let Some(conversation_id) = chosen {
        out.push(Intent::ForwardTo { conversation_id });
    } else if cancel || modal.should_close() {
        out.push(Intent::CancelForward);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_counts_read_as_words() {
        assert_eq!(selected_text(1), "1 selected");
        assert_eq!(picker_title(1), "Forward message");
        assert_eq!(picker_title(3), "Forward 3 messages");
    }
}
