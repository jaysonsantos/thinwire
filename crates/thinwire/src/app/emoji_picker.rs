//! The emoji picker for a reaction (#515), over the window.
//!
//! "More reactions" in the menu of a message opens it. It shows a search
//! field, the emoji that the user picked last ("Recent"), and the emoji of
//! `emoji_table` in groups. A click on an emoji sends the same reaction
//! intent as the emoji row of the menu, and closes the picker. Esc, Cancel,
//! and a click outside the box close it too.

use eframe::egui::{self, RichText};
use thinwire_core::{Intent, View};
use thinwire_protocol::ProtocolId;

use super::emoji_table::{Emoji, GROUPS};
use super::theme::{self, space};

/// The number of emoji in one row of the grid.
const COLUMNS: usize = 8;
/// The side of one emoji button.
const CELL: f32 = 36.0;
/// The size of the emoji in its button.
const EMOJI_SIZE: f32 = 20.0;
/// The tallest grid before it scrolls.
const GRID_HEIGHT: f32 = 320.0;

/// The message that gets the reaction, and the search text.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Open {
    protocol: ProtocolId,
    conversation_id: String,
    message_id: String,
    query: String,
}

fn open_id() -> egui::Id {
    egui::Id::unique("thinwire-emoji-picker")
}

/// Show the picker for this message.
pub(crate) fn open(
    ctx: &egui::Context,
    protocol: ProtocolId,
    conversation_id: &str,
    message_id: &str,
) {
    ctx.data_mut(|data| {
        data.insert_temp(
            open_id(),
            Open {
                protocol,
                conversation_id: conversation_id.to_owned(),
                message_id: message_id.to_owned(),
                query: String::new(),
            },
        );
    });
}

/// Close the picker, as Esc does. `true` when it was open.
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

/// The name of `emoji` in the table. An emoji that the table does not have
/// (an older build saved it) has no name.
fn name_of(emoji: &str) -> Option<&'static str> {
    GROUPS
        .iter()
        .flat_map(|group| group.emoji.iter())
        .find(|(known, _)| *known == emoji)
        .map(|(_, name)| *name)
}

/// The emoji whose name has `query`, in table order. Upper and lower case
/// are equal. An emoji as the query finds itself.
fn search(query: &str) -> Vec<Emoji> {
    let query = query.trim().to_lowercase();
    GROUPS
        .iter()
        .flat_map(|group| group.emoji.iter())
        .filter(|(emoji, name)| name.contains(&query) || *emoji == query)
        .copied()
        .collect()
}

/// One title and its emoji in rows of [`COLUMNS`]. Returns the emoji that
/// the user clicked.
fn grid<'a>(
    ui: &mut egui::Ui,
    title: Option<&str>,
    emoji: impl Iterator<Item = (&'a str, &'a str)>,
    mine: &dyn Fn(&str) -> bool,
) -> Option<String> {
    let palette = theme::palette(ui);
    if let Some(title) = title {
        ui.label(
            RichText::new(title)
                .small()
                .family(theme::semibold())
                .color(palette.text2),
        );
    }
    let cells: Vec<(&str, &str)> = emoji.collect();
    let mut picked = None;
    for row in cells.chunks(COLUMNS) {
        ui.horizontal(|ui| {
            // Each button has the side of one cell: the row has no gap
            // and the button adds no padding.
            ui.spacing_mut().item_spacing.x = 0.0;
            ui.spacing_mut().button_padding = egui::Vec2::ZERO;
            for (emoji, name) in row {
                let button =
                    egui::Button::selectable(mine(emoji), RichText::new(*emoji).size(EMOJI_SIZE))
                        .min_size(egui::vec2(CELL, CELL));
                if ui.add(button).on_hover_text(*name).clicked() {
                    picked = Some((*emoji).to_owned());
                }
            }
        });
    }
    ui.add_space(space::S);
    picked
}

/// The picker, while it is open. It closes when its message leaves the open
/// thread.
pub(crate) fn dialog(ctx: &egui::Context, snapshot: &View<'_>, out: &mut Vec<Intent>) {
    let Some(mut open) = ctx.data(|data| data.get_temp::<Open>(open_id())) else {
        return;
    };
    let message = snapshot
        .selected_messages()
        .iter()
        .find(|message| message.id == open.message_id)
        .filter(|message| {
            message.protocol == open.protocol && message.conversation_id == open.conversation_id
        });
    let Some(message) = message else {
        close(ctx);
        return;
    };
    let mine = |emoji: &str| {
        message
            .reactions
            .iter()
            .any(|reaction| reaction.mine && reaction.emoji == emoji)
    };
    let mut picked = None;
    let mut cancel = false;
    let modal = egui::Modal::new(open_id().with("modal")).show(ctx, |ui| {
        let palette = theme::palette(ui);
        ui.set_width(CELL * COLUMNS as f32 + ui.spacing().scroll.bar_width + space::S);
        ui.label(RichText::new("React with an emoji").strong());
        ui.add_space(space::S);
        let field = ui.add(
            egui::TextEdit::singleline(&mut open.query)
                .id(open_id().with("search"))
                .hint_text("Search emoji")
                .desired_width(f32::INFINITY),
        );
        super::a11y::name(ui.ctx(), field.id, "Search emoji");
        if ui.memory(|memory| memory.focused().is_none()) {
            field.request_focus();
        }
        ui.add_space(space::S);
        egui::ScrollArea::vertical()
            .max_height(GRID_HEIGHT)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                if !open.query.trim().is_empty() {
                    let found = search(&open.query);
                    if found.is_empty() {
                        ui.label(RichText::new("No emoji matches.").color(palette.text2));
                    }
                    picked = grid(ui, None, found.into_iter(), &mine);
                    return;
                }
                let recent = snapshot.recent_emoji();
                if !recent.is_empty() {
                    let cells = recent
                        .iter()
                        .map(|emoji| (emoji.as_str(), name_of(emoji).unwrap_or("recent emoji")));
                    picked = grid(ui, Some("Recent"), cells, &mine).or(picked.take());
                }
                for group in &GROUPS {
                    let cells = group.emoji.iter().copied();
                    picked = grid(ui, Some(group.title), cells, &mine).or(picked.take());
                }
            });
        ui.add_space(space::S);
        if ui.button("Cancel").clicked() {
            cancel = true;
        }
    });
    if let Some(emoji) = picked {
        let add = !mine(&emoji);
        out.push(Intent::React {
            protocol: open.protocol,
            conversation_id: open.conversation_id.clone(),
            message_id: open.message_id.clone(),
            emoji: emoji.clone(),
            add,
        });
        if add {
            out.push(Intent::UseEmoji(emoji));
        }
        close(ctx);
    } else if cancel || modal.should_close() {
        close(ctx);
    } else {
        ctx.data_mut(|data| data.insert_temp(open_id(), open));
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use eframe::egui::{FontFamily, FontId};

    use super::*;

    #[test]
    fn the_table_has_each_emoji_one_time_with_a_lower_case_name() {
        let mut seen = HashSet::new();
        let mut count = 0;
        for group in &GROUPS {
            assert!(!group.emoji.is_empty(), "{}", group.title);
            for (emoji, name) in group.emoji {
                assert!(seen.insert(*emoji), "{emoji} is in the table two times");
                assert!(!name.is_empty(), "{emoji} has no name");
                assert_eq!(*name, name.to_lowercase(), "{emoji}");
                // The settings keep a picked emoji in one line.
                assert!(
                    !emoji.chars().any(|c| c.is_alphanumeric() || c == ','),
                    "{emoji}"
                );
                count += 1;
            }
        }
        assert!(count >= 300, "{count} emoji");
    }

    /// A UI test has no font of the OS, and egui panics there for a
    /// character with no glyph. So the fonts of the app must have a glyph
    /// for each character of each emoji.
    #[test]
    fn the_app_fonts_have_a_glyph_for_each_emoji_of_the_table() {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        // The first pass builds the font atlas. No renderer runs here.
        ctx.run_ui(egui::RawInput::default(), |_| {})
            .textures_delta
            .clear();
        let font = FontId::new(EMOJI_SIZE, FontFamily::Proportional);
        let mut missing = Vec::new();
        for (emoji, name) in GROUPS.iter().flat_map(|group| group.emoji.iter()) {
            // egui draws nothing for a variation selector, so it needs no
            // glyph.
            let drawn = emoji.chars().filter(|c| *c != '\u{fe0f}');
            let found = ctx.fonts_mut(|fonts| drawn.clone().all(|c| fonts.has_glyph(&font, c)));
            if !found {
                missing.push(format!("{emoji} ({name})"));
            }
        }
        assert!(missing.is_empty(), "no glyph: {}", missing.join(", "));
    }

    #[test]
    fn the_quick_reactions_are_in_the_table() {
        for emoji in super::super::message_menu::QUICK_REACTIONS {
            assert!(name_of(emoji).is_some(), "{emoji}");
        }
    }

    #[test]
    fn the_search_reads_the_name_in_any_case() {
        let party: Vec<&str> = search(" PARTY ").iter().map(|(emoji, _)| *emoji).collect();
        assert!(party.contains(&"🎉"), "{party:?}");
        assert!(!party.contains(&"👍"));
        assert!(search("no such emoji name").is_empty());
        assert_eq!(search("🎉").len(), 1);
    }
}
