use eframe::egui::{self, RichText};
use thinwire::update::{Action, PlainError, Prompt, Updates};

use super::theme::space;

pub(crate) fn draw(ui: &mut egui::Ui, updates: &Updates) {
    match updates.prompt() {
        Prompt::Quiet => {}
        Prompt::Installing | Prompt::QuitToInstall => installing(ui),
        Prompt::Offer(offer) => offer_modal(ui, updates, offer.version(), offer.notes()),
        Prompt::Failed(error) => failed(ui, updates, error),
    }
}

fn installing(ui: &mut egui::Ui) {
    egui::Modal::new(egui::Id::unique("thinwire-update-installing")).show(ui.ctx(), |ui| {
        ui.set_width(360.0);
        ui.label("Installing the update. Thinwire will reopen.");
    });
}

fn offer_modal(ui: &mut egui::Ui, updates: &Updates, version: &str, notes: &str) {
    let mut action = None;
    let modal = egui::Modal::new(egui::Id::unique("thinwire-update-offer")).show(ui.ctx(), |ui| {
        ui.set_width(360.0);
        ui.label(RichText::new(version).strong());
        ui.add_space(space::S);
        ui.label(notes);
        ui.add_space(space::M);
        ui.horizontal(|ui| {
            if ui.button("Install").clicked() {
                action = Some(Action::Install);
            }
            if ui.button("Later").clicked() {
                action = Some(Action::Later);
            }
        });
    });
    if action.is_none() && modal.should_close() {
        action = Some(Action::Later);
    }
    if let Some(action) = action {
        updates.act(action);
    }
}

fn failed(ui: &mut egui::Ui, updates: &Updates, error: PlainError) {
    let mut action = None;
    let modal = egui::Modal::new(egui::Id::unique("thinwire-update-failed")).show(ui.ctx(), |ui| {
        ui.set_width(360.0);
        ui.label(error.text);
        ui.add_space(space::M);
        let label = if error.retry { "Try again" } else { "Dismiss" };
        if ui.button(label).clicked() {
            action = Some(if error.retry {
                Action::Retry
            } else {
                Action::Dismiss
            });
        }
    });
    if action.is_none() && modal.should_close() {
        action = Some(Action::Dismiss);
    }
    if let Some(action) = action {
        updates.act(action);
    }
}
