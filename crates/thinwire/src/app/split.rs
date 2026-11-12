//! The split view: two panes side by side, each with the chat list and the
//! thread of one account.
//!
//! The core has one selection, the pane with the focus. The app draws that
//! pane first ([`super::ui::draw`]) and then the other pane with a view of
//! its own (`Core::with_other_pane`). This module has the two rectangles,
//! the divider between them, and the press that gives a pane the focus.
//! "Open in split" and "Close split" are in the menu of an account row
//! (`super::unlink`).

use eframe::egui;
use thinwire_core::SplitSide;

use super::theme;

/// Width of the zone that drags the divider.
const GRAB: f32 = 8.0;
/// The least share of the width that a pane keeps.
const MIN_SHARE: f32 = 0.3;
/// Height of the line that marks the pane with the focus.
const FOCUS_LINE: f32 = 2.0;

fn open_id() -> egui::Id {
    egui::Id::unique("thinwire-split-open")
}

fn share_id() -> egui::Id {
    egui::Id::unique("thinwire-split-share")
}

fn other_id() -> egui::Id {
    egui::Id::unique("thinwire-split-other")
}

/// Keep for this frame that the split view is open: the menu of an account
/// row has no view.
pub(crate) fn remember(ctx: &egui::Context, open: bool) {
    ctx.data_mut(|data| {
        data.insert_temp(open_id(), open);
        if !open {
            data.remove::<(egui::Rect, bool)>(other_id());
        }
    });
}

/// The split view is open, as [`remember`] last said.
pub(crate) fn is_open(ctx: &egui::Context) -> bool {
    ctx.data(|data| data.get_temp::<bool>(open_id()))
        .unwrap_or(false)
}

/// Divide the free space of `ui` into two panes and draw the divider. A
/// drag of the divider changes the share of each pane. Returns the
/// rectangle of the pane with the focus, on the side `active`, and keeps
/// the other rectangle for [`other`].
pub(crate) fn layout(ui: &mut egui::Ui, active: SplitSide) -> egui::Rect {
    let full = ui.available_rect_before_wrap();
    let mut share = ui
        .data(|data| data.get_temp::<f32>(share_id()))
        .unwrap_or(0.5);
    let grab = egui::Rect::from_center_size(
        egui::pos2(full.left() + full.width() * share, full.center().y),
        egui::vec2(GRAB, full.height()),
    );
    let handle = ui
        .interact(grab, share_id(), egui::Sense::drag())
        .on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
    if handle.dragged() && full.width() > 0.0 {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        share += handle.drag_delta().x / full.width();
    }
    let share = share.clamp(MIN_SHARE, 1.0 - MIN_SHARE);
    ui.data_mut(|data| data.insert_temp(share_id(), share));
    let middle = (full.left() + full.width() * share).round();
    let left = egui::Rect::from_min_max(full.min, egui::pos2(middle, full.bottom()));
    let right = egui::Rect::from_min_max(egui::pos2(middle + 1.0, full.top()), full.max);
    let palette = theme::palette(ui);
    ui.painter().vline(
        middle + 0.5,
        full.y_range(),
        egui::Stroke::new(1.0, palette.border),
    );
    let (mine, theirs) = match active {
        SplitSide::Left => (left, right),
        SplitSide::Right => (right, left),
    };
    let theirs_left = active == SplitSide::Right;
    ui.data_mut(|data| data.insert_temp(other_id(), (theirs, theirs_left)));
    mine
}

/// The rectangle and the side of the pane that does not have the focus,
/// from the [`layout`] of this frame.
pub(crate) fn other(ctx: &egui::Context) -> Option<(egui::Rect, SplitSide)> {
    let (rect, left) = ctx.data(|data| data.get_temp::<(egui::Rect, bool)>(other_id()))?;
    let side = if left {
        SplitSide::Left
    } else {
        SplitSide::Right
    };
    Some((rect, side))
}

/// Draw one pane in `rect`. The widgets of a side keep their ids when the
/// focus changes, so a scroll position stays with its side. The pane with
/// the focus has a line in the accent color at its top.
pub(crate) fn pane(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    side: SplitSide,
    focused: bool,
    add: impl FnOnce(&mut egui::Ui),
) {
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .id_salt(("thinwire-split-pane", side == SplitSide::Left)),
    );
    child.set_clip_rect(rect.intersect(ui.clip_rect()));
    add(&mut child);
    if focused {
        let line = egui::Rect::from_min_size(rect.min, egui::vec2(rect.width(), FOCUS_LINE));
        ui.painter()
            .rect_filled(line, 0.0, theme::palette(ui).accent);
    }
}

/// The user pressed a mouse button in `rect`, and no menu or box of the
/// window is over that point. The zone of the divider does not count.
pub(crate) fn pressed(ctx: &egui::Context, rect: egui::Rect) -> bool {
    let pos = ctx.input(|input| {
        input
            .pointer
            .any_pressed()
            .then(|| input.pointer.interact_pos())
            .flatten()
    });
    let Some(pos) = pos else {
        return false;
    };
    rect.shrink2(egui::vec2(GRAB / 2.0, 0.0)).contains(pos)
        && ctx
            .layer_id_at(pos)
            .is_none_or(|layer| layer.order == egui::Order::Background)
}
