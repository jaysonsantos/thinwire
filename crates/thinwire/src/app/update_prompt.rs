use eframe::egui::{self, RichText};
use thinwire::update::{Action, NIGHTLY_PAGE, PlainError, Prompt, Updates};

use super::theme::space;

pub(crate) fn draw(ui: &mut egui::Ui, updates: &Updates) {
    match updates.prompt() {
        Prompt::Quiet => {}
        Prompt::Checking => checking(ui, updates),
        Prompt::UpToDate(build) => up_to_date(ui, updates, &build),
        Prompt::Installing | Prompt::QuitToInstall => installing(ui),
        Prompt::Offer(offer) => offer_modal(ui, updates, offer.version(), offer.notes()),
        Prompt::Available(offer) => available(ui, updates, offer.version(), offer.notes()),
        Prompt::Failed(error) => failed(ui, updates, error),
    }
}

/// The text of a manual check that found no newer build.
pub(crate) fn up_to_date_text(build: &str) -> String {
    format!("Thinwire is up to date (build {build}).")
}

/// A manual check runs. Hide keeps it running: only a newer build shows.
fn checking(ui: &mut egui::Ui, updates: &Updates) {
    let mut hide = false;
    let modal =
        egui::Modal::new(egui::Id::unique("thinwire-update-checking")).show(ui.ctx(), |ui| {
            ui.set_width(360.0);
            ui.label("Checking for updates…");
            ui.add_space(space::M);
            hide = ui.button("Hide").clicked();
        });
    if hide || modal.should_close() {
        updates.act(Action::Dismiss);
    }
}

fn up_to_date(ui: &mut egui::Ui, updates: &Updates, build: &str) {
    let mut close = false;
    let modal =
        egui::Modal::new(egui::Id::unique("thinwire-update-current")).show(ui.ctx(), |ui| {
            ui.set_width(360.0);
            ui.label(up_to_date_text(build));
            ui.add_space(space::M);
            close = ui.button("OK").clicked();
        });
    if close || modal.should_close() {
        updates.act(Action::Dismiss);
    }
}

/// Linux and Windows: a newer build is on the nightly release page. The
/// app does not download it.
fn available(ui: &mut egui::Ui, updates: &Updates, version: &str, notes: &str) {
    let mut action = None;
    let modal =
        egui::Modal::new(egui::Id::unique("thinwire-update-available")).show(ui.ctx(), |ui| {
            ui.set_width(360.0);
            ui.label(RichText::new(version).strong());
            ui.add_space(space::S);
            ui.label(notes);
            ui.add_space(space::M);
            ui.horizontal(|ui| {
                if ui.button("Open download page").clicked() {
                    super::links::open_url(NIGHTLY_PAGE);
                    action = Some(Action::Later);
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
