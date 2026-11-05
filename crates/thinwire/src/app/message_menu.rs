//! The menu of one message: a right click on its bubble. It offers only what
//! the protocol of the message allows (`MessageActions`): reply, an emoji
//! reaction, copy, and delete. The reactions of a message show under its
//! bubble, and a reply shows the message it answers.

use eframe::egui::{self, RichText};
use thinwire_core::{Intent, View};
use thinwire_protocol::{ChatMessage, MessageActions, ProtocolId, Reaction};

use super::theme::{self, radius, space};

/// The emoji row of the menu.
pub(crate) const QUICK_REACTIONS: [&str; 6] = ["👍", "❤️", "😂", "😮", "😢", "🙏"];

/// The longest quote of a replied message, in characters.
const QUOTE_CHARS: usize = 80;

/// The width of the accent bar beside a quote.
const QUOTE_BAR: f32 = 3.0;

/// One message, as the menu needs it.
pub(crate) struct Target<'a> {
    pub(crate) protocol: ProtocolId,
    pub(crate) conversation_id: &'a str,
    pub(crate) message_id: &'a str,
    pub(crate) body: &'a str,
    pub(crate) outbound: bool,
    pub(crate) reactions: &'a [Reaction],
}

impl Target<'_> {
    fn react(&self, emoji: &str, add: bool) -> Intent {
        Intent::React {
            protocol: self.protocol,
            conversation_id: self.conversation_id.to_owned(),
            message_id: self.message_id.to_owned(),
            emoji: emoji.to_owned(),
            add,
        }
    }

    fn mine(&self, emoji: &str) -> bool {
        self.reactions
            .iter()
            .any(|reaction| reaction.mine && reaction.emoji == emoji)
    }
}

/// The delete choices of one message: for this account, for every member,
/// or both.
fn delete_choices(actions: MessageActions, outbound: bool) -> (bool, bool) {
    (
        actions.delete_for_me,
        actions.delete_for_everyone && outbound,
    )
}

/// The menu opens on a right click anywhere in `bubble`. The text, the links,
/// and the photo keep their own clicks.
pub(crate) fn attach(
    bubble: &egui::Response,
    target: &Target<'_>,
    actions: MessageActions,
    out: &mut Vec<Intent>,
) {
    bubble.container_context_menu(|ui| menu(ui, target, actions, out));
}

fn menu(ui: &mut egui::Ui, target: &Target<'_>, actions: MessageActions, out: &mut Vec<Intent>) {
    if actions.react {
        ui.horizontal(|ui| {
            for emoji in QUICK_REACTIONS {
                let mine = target.mine(emoji);
                let response = ui
                    .add(egui::Button::selectable(
                        mine,
                        RichText::new(emoji).size(18.0),
                    ))
                    .on_hover_text(if mine { "Remove reaction" } else { "React" });
                if response.clicked() {
                    out.push(target.react(emoji, !mine));
                    ui.close();
                }
            }
        });
        ui.separator();
    }
    if actions.reply && ui.button("Reply").clicked() {
        out.push(Intent::ReplyTo {
            protocol: target.protocol,
            conversation_id: target.conversation_id.to_owned(),
            message_id: target.message_id.to_owned(),
        });
        ui.memory_mut(|memory| memory.request_focus(egui::Id::unique("thread-compose")));
        ui.close();
    }
    if !target.body.is_empty() && ui.button("Copy text").clicked() {
        ui.ctx().copy_text(target.body.to_owned());
        ui.close();
    }
    let (me, everyone) = delete_choices(actions, target.outbound);
    if (me || everyone) && ui.button("Delete…").clicked() {
        let ask = DeleteAsk {
            protocol: target.protocol,
            conversation_id: target.conversation_id.to_owned(),
            message_id: target.message_id.to_owned(),
            me,
            everyone,
            for_everyone: !me,
        };
        ui.ctx().data_mut(|data| data.insert_temp(ask_id(), ask));
        ui.close();
    }
}

/// The reactions under a bubble. A click on one adds or removes the reaction
/// of this account, when the protocol allows it.
pub(crate) fn reactions(
    ui: &mut egui::Ui,
    target: &Target<'_>,
    actions: MessageActions,
    out: &mut Vec<Intent>,
) {
    if target.reactions.is_empty() {
        return;
    }
    let palette = theme::palette(ui);
    let layout = if target.outbound {
        egui::Layout::right_to_left(egui::Align::Center)
    } else {
        egui::Layout::left_to_right(egui::Align::Center)
    };
    ui.add_space(space::XS / 2.0);
    ui.allocate_ui_with_layout(
        egui::vec2(
            ui.available_width(),
            ui.text_style_height(&egui::TextStyle::Body),
        ),
        layout,
        |ui| {
            ui.spacing_mut().item_spacing.x = space::XS;
            let ordered: Vec<&Reaction> = if target.outbound {
                target.reactions.iter().rev().collect()
            } else {
                target.reactions.iter().collect()
            };
            for reaction in ordered {
                let label = if reaction.count > 1 {
                    format!("{} {}", reaction.emoji, reaction.count)
                } else {
                    reaction.emoji.clone()
                };
                let chip = egui::Button::new(RichText::new(label).small().color(palette.text))
                    .selected(reaction.mine)
                    .fill(palette.surface)
                    .corner_radius(radius::PILL)
                    .small();
                let response = ui.add(chip);
                if actions.react && response.clicked() {
                    out.push(target.react(&reaction.emoji, !reaction.mine));
                }
            }
        },
    );
}

/// The message that a reply answers, at the top of its bubble.
pub(crate) fn quote(ui: &mut egui::Ui, replied: Option<&ChatMessage>, outbound: bool) {
    let palette = theme::palette(ui);
    let (bar, name, text) = if outbound {
        (palette.on_out, palette.on_out, palette.out_meta)
    } else {
        (palette.accent, palette.accent, palette.text2)
    };
    let (sender, snippet) = match replied {
        Some(message) => (message.sender.as_str(), snippet(&message.body)),
        None => ("", "An older message".to_owned()),
    };
    lined(ui, bar, |ui| {
        if !sender.is_empty() {
            ui.label(
                RichText::new(sender)
                    .small()
                    .family(theme::semibold())
                    .color(name),
            );
        }
        ui.add(egui::Label::new(RichText::new(snippet).small().color(text)).truncate());
    });
    ui.add_space(space::XS);
}

/// The reply line over the compose field: the message the next send answers,
/// and a button that cancels the reply.
pub(crate) fn reply_bar(ui: &mut egui::Ui, snapshot: &View<'_>, out: &mut Vec<Intent>) {
    let Some(message) = snapshot.reply_target() else {
        return;
    };
    let palette = theme::palette(ui);
    ui.horizontal(|ui| {
        let width = (ui.available_width() - 32.0).max(0.0);
        ui.allocate_ui_with_layout(
            egui::vec2(width, reply_bar_height(ui, snapshot)),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                lined(ui, palette.accent, |ui| {
                    ui.label(
                        RichText::new(format!("Reply to {}", message.sender))
                            .small()
                            .family(theme::semibold())
                            .color(palette.accent),
                    );
                    ui.add(
                        egui::Label::new(
                            RichText::new(snippet(&message.body))
                                .small()
                                .color(palette.text2),
                        )
                        .truncate(),
                    );
                });
            },
        );
        if ui
            .add(egui::Button::new("✕").frame(false))
            .on_hover_text("Cancel reply")
            .clicked()
        {
            out.push(Intent::CancelReply);
        }
    });
    ui.add_space(space::XS);
}

/// The height of [`reply_bar`]: nothing when no reply is open.
pub(crate) fn reply_bar_height(ui: &egui::Ui, snapshot: &View<'_>) -> f32 {
    if snapshot.reply_target().is_none() {
        return 0.0;
    }
    let line = ui.text_style_height(&egui::TextStyle::Small);
    line * 2.0 + ui.spacing().item_spacing.y + space::XS
}

/// Text in a column with a thin bar on its left.
fn lined(ui: &mut egui::Ui, bar: egui::Color32, add: impl FnOnce(&mut egui::Ui)) {
    let response = ui
        .horizontal(|ui| {
            ui.add_space(QUOTE_BAR + space::S);
            ui.vertical(add);
        })
        .response;
    let rect = response.rect;
    ui.painter().rect_filled(
        egui::Rect::from_min_size(rect.min, egui::vec2(QUOTE_BAR, rect.height())),
        egui::CornerRadius::same(1),
        bar,
    );
}

/// The first line of `body`, at most [`QUOTE_CHARS`] characters.
fn snippet(body: &str) -> String {
    let line = body
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    let line = line.trim();
    if line.is_empty() {
        return "Media".to_owned();
    }
    let mut text: String = line.chars().take(QUOTE_CHARS).collect();
    if line.chars().count() > QUOTE_CHARS {
        text.push('…');
    }
    text
}

/// The delete question of one message, while it is open.
#[derive(Clone, Debug)]
struct DeleteAsk {
    protocol: ProtocolId,
    conversation_id: String,
    message_id: String,
    /// Delete for this account is possible.
    me: bool,
    /// Delete for every member is possible.
    everyone: bool,
    /// The state of the "Also delete for everyone" box.
    for_everyone: bool,
}

fn ask_id() -> egui::Id {
    egui::Id::unique("thinwire-delete-ask")
}

/// Close the delete question, as Esc does. `true` when it was open.
pub(crate) fn take_escape(ctx: &egui::Context) -> bool {
    let open = ctx.data(|data| data.get_temp::<DeleteAsk>(ask_id()).is_some());
    if open {
        ctx.data_mut(|data| data.remove::<DeleteAsk>(ask_id()));
    }
    open
}

/// The delete question over the window, when the menu opened it.
pub(crate) fn delete_dialog(ctx: &egui::Context, out: &mut Vec<Intent>) {
    let Some(mut ask) = ctx.data(|data| data.get_temp::<DeleteAsk>(ask_id())) else {
        return;
    };
    let mut answer = None;
    let modal = egui::Modal::new(ask_id().with("modal")).show(ctx, |ui| {
        ui.set_width(300.0);
        ui.label(RichText::new("Delete this message?").strong());
        ui.add_space(space::S);
        if ask.me && ask.everyone {
            ui.checkbox(&mut ask.for_everyone, "Also delete for everyone");
        } else if ask.everyone {
            ui.label("Every member of the chat loses it.");
        } else {
            ui.label("Only this account loses it. The others keep it.");
        }
        ui.add_space(space::M);
        ui.horizontal(|ui| {
            if ui.button("Delete").clicked() {
                answer = Some(true);
            }
            if ui.button("Cancel").clicked() {
                answer = Some(false);
            }
        });
    });
    if answer.is_none() && modal.should_close() {
        answer = Some(false);
    }
    match answer {
        Some(delete) => {
            if delete {
                out.push(Intent::DeleteMessage {
                    protocol: ask.protocol,
                    conversation_id: ask.conversation_id.clone(),
                    message_id: ask.message_id.clone(),
                    for_everyone: ask.for_everyone || !ask.me,
                });
            }
            ctx.data_mut(|data| data.remove::<DeleteAsk>(ask_id()));
        }
        None => {
            ctx.data_mut(|data| data.insert_temp(ask_id(), ask));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_snippet_is_the_first_line_and_short() {
        assert_eq!(snippet("\nHello\nworld"), "Hello");
        assert_eq!(snippet(""), "Media");
        let long = "a".repeat(QUOTE_CHARS + 5);
        assert_eq!(snippet(&long).chars().count(), QUOTE_CHARS + 1);
    }

    #[test]
    fn delete_for_everyone_needs_an_own_message() {
        assert_eq!(delete_choices(MessageActions::ALL, false), (true, false));
        assert_eq!(delete_choices(MessageActions::ALL, true), (true, true));
        assert_eq!(delete_choices(MessageActions::NONE, true), (false, false));
    }
}
