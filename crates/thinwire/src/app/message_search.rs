//! The "Messages" section of the chat list (#514): the messages that the
//! account found for the text of the search field.
//!
//! The core has the state (`View::message_search`). This module draws it
//! under the chat rows: a heading, then "Searching…", "No messages found",
//! or one row for each found message. A row has the look of a chat row:
//! the picture and the name of the chat, the time, and one line of the
//! text around the match. A click sends `Intent::OpenFoundMessage`.

use eframe::egui::{self, RichText};
use thinwire_core::{Intent, MessageHit, MessageSearchView, View};

use super::theme::{self, space};

/// The heading of the section.
pub(super) const HEADING: &str = "Messages";
/// The line while a request waits for its time or for its answer.
pub(super) const SEARCHING: &str = "Searching…";
/// The line of a search with no message.
pub(super) const NONE_FOUND: &str = "No messages found";

/// The second line of a result row: the sender, then the text. A group
/// names the sender. A chat with one person names only this account.
fn line(hit: &MessageHit<'_>) -> String {
    let sender = if hit.message.outbound {
        "You"
    } else if hit.chat.is_group {
        hit.message.sender.trim()
    } else {
        ""
    };
    if sender.is_empty() {
        hit.snippet.clone()
    } else {
        format!("{sender}: {}", hit.snippet)
    }
}

/// The name of a result row for a screen reader: what it is, the chat, the
/// account when two accounts of the protocol are in the list, and the
/// line.
fn row_name(hit: &MessageHit<'_>, line: &str) -> String {
    let mut name = format!("Message in {}", hit.chat.title);
    if let Some(account) = &hit.account {
        name.push_str(&format!(", {account}"));
    }
    name.push_str(&format!(", {line}"));
    name
}

/// A quiet line of the section, in line with the row content.
fn note(ui: &mut egui::Ui, text: &str) {
    let palette = theme::palette(ui);
    ui.horizontal(|ui| {
        ui.add_space(theme::ROW_INSET.x);
        ui.label(RichText::new(text).color(palette.text3));
    });
}

/// Draw the section at the end of the chat list. Nothing shows when the
/// list has no message search.
pub(super) fn show(ui: &mut egui::Ui, snapshot: &View<'_>, out: &mut Vec<Intent>) {
    let found = snapshot.message_search();
    if matches!(found, MessageSearchView::Off) {
        return;
    }
    let palette = theme::palette(ui);
    ui.add_space(space::M);
    ui.horizontal(|ui| {
        ui.add_space(theme::ROW_INSET.x);
        ui.label(
            RichText::new(HEADING)
                .small()
                .family(theme::semibold())
                .color(palette.text2),
        );
    });
    ui.add_space(space::XS);
    let hits = match found {
        MessageSearchView::Off => return,
        MessageSearchView::Searching => return note(ui, SEARCHING),
        MessageSearchView::Empty => return note(ui, NONE_FOUND),
        MessageSearchView::Found(hits) => hits,
    };
    let now = snapshot.now();
    let mut clicked = None;
    for hit in &hits {
        let line = line(hit);
        let time = super::ui::view_list_time(hit.message.sent_at, now);
        // The account and the message id: two accounts can have a message
        // with the same id.
        let key = format!(
            "found\u{1f}{}\u{1f}{}",
            hit.protocol.key(),
            hit.message.message_id
        );
        let response = super::ui::inbox_row(
            ui,
            hit.protocol,
            &key,
            hit.account.as_deref(),
            &hit.chat.id,
            &hit.chat.title,
            &line,
            &time,
            0,
            false,
            false,
            &hit.chat.avatar,
            out,
        );
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::Role::Button, true, row_name(hit, &line))
        });
        if response.clicked() {
            clicked = Some(hit);
        }
    }
    if let Some(hit) = clicked {
        out.push(Intent::OpenFoundMessage {
            protocol: hit.protocol,
            conversation_id: hit.message.conversation_id.clone(),
            topic: hit.message.topic,
            message_id: hit.message.message_id.clone(),
        });
    }
}
