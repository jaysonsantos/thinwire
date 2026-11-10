//! The menu of an account row in the sidebar: move the account, or unlink
//! it. The question before the unlink is here too.
//!
//! A right click on an account row, or on its tile in the collapsed
//! sidebar, opens a menu with "Move up" and "Move down". A linked account
//! also has "Unlink…". The Menu key and Shift+F10 open the same menu for the
//! row that has the keyboard focus. "Unlink…" opens the question. Only
//! "Unlink" there sends [`Intent::UnlinkAccount`].

use eframe::egui::{self, RichText};
use thinwire_core::Intent;
use thinwire_core::state::{AccountRow, unlink_notice};
use thinwire_protocol::ProtocolId;

use super::theme::space;

/// The unlink question of one account, while it is open.
#[derive(Clone, Debug)]
struct UnlinkAsk {
    account: ProtocolId,
}

/// A move that the menu asked for. [`dialog`] sends it in the same frame.
#[derive(Clone, Copy, Debug)]
struct MoveAsk {
    account: ProtocolId,
    up: bool,
}

fn move_id() -> egui::Id {
    egui::Id::unique("thinwire-account-move")
}

fn ask_id() -> egui::Id {
    egui::Id::unique("thinwire-unlink-ask")
}

/// The Menu key, or Shift+F10: the keys that open a context menu.
fn menu_key(ctx: &egui::Context) -> bool {
    ctx.input(|input| {
        input.key_pressed(egui::Key::Menu)
            || (input.modifiers.shift && input.key_pressed(egui::Key::F10))
    })
}

/// Give the account row `row` its menu. A previous Signal number is not an
/// account, so it has no menu. Only a linked account has "Unlink…".
pub(crate) fn attach(row: &egui::Response, account: &AccountRow) {
    if account.signal_retained.is_some() {
        return;
    }
    let linked = account.linked();
    let id = account.caps.id;
    // A menu that the keyboard opened has no pointer position: it stays
    // at its row.
    let by_key_id = row.id.with("unlink-menu-by-key");
    let opens = row.has_focus() && menu_key(&row.ctx);
    if opens {
        row.ctx.data_mut(|data| data.insert_temp(by_key_id, true));
    }
    let by_key = row
        .ctx
        .data(|data| data.get_temp::<bool>(by_key_id))
        .unwrap_or(false);
    let mut popup = egui::Popup::context_menu(row);
    if by_key {
        popup = popup.anchor(row);
    }
    if opens {
        popup = popup.open_memory(Some(egui::SetOpenCommand::Bool(true)));
    }
    let shown = popup.show(|ui| menu(ui, id, linked));
    if by_key && !opens && shown.is_none() {
        row.ctx.data_mut(|data| data.remove::<bool>(by_key_id));
    }
}

fn menu(ui: &mut egui::Ui, account: ProtocolId, linked: bool) {
    for (label, up) in [("Move up", true), ("Move down", false)] {
        let item = ui.button(label);
        item.widget_info(|| {
            egui::WidgetInfo::labeled(
                egui::Role::Button,
                true,
                format!("{label}: {}", account.label()),
            )
        });
        if item.clicked() {
            ui.ctx()
                .data_mut(|data| data.insert_temp(move_id(), MoveAsk { account, up }));
            ui.close();
        }
    }
    if !linked {
        return;
    }
    ui.separator();
    let item = ui.button("Unlink…");
    // The name has the account, so a screen reader does not read only
    // "Unlink".
    item.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::Role::Button,
            true,
            format!("Unlink {}", account.label()),
        )
    });
    if item.clicked() {
        ui.ctx()
            .data_mut(|data| data.insert_temp(ask_id(), UnlinkAsk { account }));
        ui.close();
    }
}

/// Close the unlink question, as Esc does. `true` when it was open.
pub(crate) fn take_escape(ctx: &egui::Context) -> bool {
    let open = ctx.data(|data| data.get_temp::<UnlinkAsk>(ask_id()).is_some());
    if open {
        ctx.data_mut(|data| data.remove::<UnlinkAsk>(ask_id()));
    }
    open
}

/// The unlink question over the window, when the menu opened it. Cancel
/// and Esc change nothing.
pub(crate) fn dialog(ctx: &egui::Context, out: &mut Vec<Intent>) {
    let moved = ctx.data(|data| data.get_temp::<MoveAsk>(move_id()));
    if let Some(ask) = moved {
        ctx.data_mut(|data| data.remove::<MoveAsk>(move_id()));
        out.push(Intent::MoveAccount {
            account: ask.account,
            up: ask.up,
        });
    }
    let Some(ask) = ctx.data(|data| data.get_temp::<UnlinkAsk>(ask_id())) else {
        return;
    };
    let mut answer = None;
    let modal = egui::Modal::new(ask_id().with("modal")).show(ctx, |ui| {
        ui.set_width(320.0);
        ui.label(RichText::new(format!("Unlink {}?", ask.account.label())).strong());
        ui.add_space(space::S);
        ui.label(unlink_notice(ask.account.kind()));
        ui.add_space(space::M);
        ui.horizontal(|ui| {
            if ui.button("Unlink").clicked() {
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
    if let Some(unlink) = answer {
        if unlink {
            out.push(Intent::UnlinkAccount(ask.account));
        }
        ctx.data_mut(|data| data.remove::<UnlinkAsk>(ask_id()));
    }
}
