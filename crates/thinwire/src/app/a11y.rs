//! Names in the screen-reader tree (AccessKit). A screen reader, and an
//! automation tool that uses AT-SPI, UI Automation, or the macOS
//! accessibility API, finds a widget by this name.
//!
//! A text field with only a placeholder has no name: AT-SPI shows the
//! placeholder as an attribute only. On Linux, AT-SPI reads the name of a
//! `Label` node from its value, not from its label.

use eframe::egui;

/// Give the widget `id` the accessible name `name`. Call it after the
/// widget is added in this frame.
pub(crate) fn name(ctx: &egui::Context, id: egui::Id, name: &str) {
    let name = name.to_owned();
    ctx.accesskit_node_builder(id, |node| node.set_label(name));
}

/// Give the label `id` the word `word` as its name, also where AT-SPI reads
/// the value. The painted text (for example a check mark) does not change.
pub(crate) fn name_label(ctx: &egui::Context, id: egui::Id, word: &str) {
    let word = word.to_owned();
    ctx.accesskit_node_builder(id, |node| {
        node.set_label(word.clone());
        node.set_value(word);
    });
}

/// Make the node `id` a polite live region: a screen reader reads a change
/// of its text when the user does not type or read.
pub(crate) fn live(ctx: &egui::Context, id: egui::Id) {
    ctx.accesskit_node_builder(id, |node| node.set_live(egui::accesskit::Live::Polite));
}
