//! The details of the open chat, over the window.
//!
//! A click on the picture in the thread header opens it. It shows the
//! picture, the name, the kind of chat, its account, and its notification
//! state, and it mutes or unmutes the chat in thinwire. Close, Esc, and a
//! click outside the box close it. It shows only what the core has: a
//! member list and a phone number are not there yet.

use eframe::egui::{self, RichText};
use thinwire_core::mutes::ChatMute;
use thinwire_core::{Intent, View};
use thinwire_protocol::ProtocolId;

use super::theme::{self, space};

/// Side of the picture in the box.
const PICTURE_SIDE: f32 = 96.0;
/// Width of the box.
const WIDTH: f32 = 320.0;

/// The chat whose details show.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Open {
    protocol: ProtocolId,
    conversation_id: String,
}

fn open_id() -> egui::Id {
    egui::Id::unique("thinwire-chat-details")
}

/// Show the details of this chat.
pub(crate) fn open(ctx: &egui::Context, protocol: ProtocolId, conversation_id: &str) {
    ctx.data_mut(|data| {
        data.insert_temp(
            open_id(),
            Open {
                protocol,
                conversation_id: conversation_id.to_owned(),
            },
        );
    });
}

/// Close the details, as Esc does. `true` when they were open.
pub(crate) fn take_escape(ctx: &egui::Context) -> bool {
    let open = ctx.data(|data| data.get_temp::<Open>(open_id()).is_some());
    if open {
        close(ctx);
    }
    open
}

fn close(ctx: &egui::Context) {
    ctx.data_mut(|data| data.remove::<Open>(open_id()));
}

/// The notification state of the chat in words.
fn mute_line(mute: ChatMute, protocol: ProtocolId) -> String {
    match mute {
        ChatMute::None => "Notifications are on.".to_owned(),
        ChatMute::Here => "Muted in thinwire.".to_owned(),
        ChatMute::Protocol => format!(
            "Muted in {}. Unmute it there to get notifications.",
            protocol.first_account().label()
        ),
    }
}

/// The details box, while it is open. It closes when the user opens
/// another chat.
pub(crate) fn dialog(ctx: &egui::Context, snapshot: &View<'_>, out: &mut Vec<Intent>) {
    let Some(open) = ctx.data(|data| data.get_temp::<Open>(open_id())) else {
        return;
    };
    let Some(conversation) = snapshot
        .selected_conversation_row()
        .filter(|row| row.protocol == open.protocol && row.id == open.conversation_id)
    else {
        close(ctx);
        return;
    };
    let mute = snapshot.chat_mute(conversation.protocol, &conversation.id);
    let mut done = false;
    let modal = egui::Modal::new(open_id().with("modal")).show(ctx, |ui| {
        let palette = theme::palette(ui);
        ui.set_width(WIDTH);
        ui.vertical_centered(|ui| {
            let (picture, _) = ui
                .allocate_exact_size(egui::vec2(PICTURE_SIDE, PICTURE_SIDE), egui::Sense::hover());
            super::photos::paint_avatar(
                ui,
                picture,
                conversation.protocol,
                &conversation.id,
                &conversation.title,
                &conversation.avatar,
                palette.bg,
                out,
            );
            ui.add_space(space::S);
            ui.label(
                RichText::new(&conversation.title)
                    .heading()
                    .color(palette.text),
            );
            let kind = if conversation.is_group {
                "Group"
            } else {
                "Person"
            };
            ui.label(
                RichText::new(format!("{kind} · {}", conversation.protocol.label()))
                    .text_style(theme::secondary())
                    .color(palette.text2),
            );
        });
        ui.add_space(space::M);
        ui.label(RichText::new(mute_line(mute, conversation.protocol)).color(palette.text2));
        if mute != ChatMute::Protocol {
            let mut muted = mute == ChatMute::Here;
            if ui.checkbox(&mut muted, "Mute in thinwire").changed() {
                out.push(Intent::SetChatMute {
                    protocol: conversation.protocol,
                    conversation_id: conversation.id.clone(),
                    muted,
                });
            }
        }
        if !conversation.writable {
            ui.label(RichText::new("You cannot send messages in this chat.").color(palette.text2));
        }
        if conversation.has_topics {
            ui.label(RichText::new("This chat has topics.").color(palette.text2));
        }
        ui.add_space(space::M);
        if ui.button("Close").clicked() {
            done = true;
        }
    });
    if done || modal.should_close() {
        close(ctx);
    }
}
