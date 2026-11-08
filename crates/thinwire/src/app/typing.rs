//! The typing line over the compose field (#510): "Ada is typing…". The
//! core keeps who types. This module draws the line.

use eframe::egui::{self, RichText};
use thinwire_core::View;

use super::theme::{self, space};

/// The line over the compose field while a person types in the open thread.
/// A screen reader reads a change of it.
pub(crate) fn line(ui: &mut egui::Ui, snapshot: &View<'_>) {
    let Some(text) = snapshot.typing_line() else {
        return;
    };
    let palette = theme::palette(ui);
    let response = ui.add(
        egui::Label::new(RichText::new(&text).small().italics().color(palette.text2)).truncate(),
    );
    super::a11y::name_label(ui.ctx(), response.id, &text);
    super::a11y::live(ui.ctx(), response.id);
    ui.add_space(space::XS);
}

/// The height of [`line`]: nothing when nobody types.
pub(crate) fn line_height(ui: &egui::Ui, snapshot: &View<'_>) -> f32 {
    if snapshot.typing_line().is_none() {
        return 0.0;
    }
    ui.text_style_height(&egui::TextStyle::Small) + ui.spacing().item_spacing.y + space::XS
}
