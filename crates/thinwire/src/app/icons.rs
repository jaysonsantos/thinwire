//! Icons the shell draws itself. Inter and the egui fallback fonts miss several
//! symbol glyphs, so each mark is painted here. Every icon fits a square of
//! `size` centered on `center`. Stroke icons and filled protocol marks share
//! this module.
//!
//! Protocol marks are original drawings that only resemble the common shapes.
//! They are not the brand assets.

use eframe::egui::{self, Color32, Pos2, Shape, Stroke, Vec2};
use thinwire_protocol::{ProtocolId, ProtocolKind};

fn at(center: Pos2, unit: f32, x: f32, y: f32) -> Pos2 {
    center + Vec2::new(x, y) * unit
}

/// A contrasting cut color for details drawn on top of `fill`.
fn cut_on(fill: Color32) -> Color32 {
    let luma = u16::from(fill.r()) + u16::from(fill.g()) + u16::from(fill.b());
    if luma > 400 {
        Color32::from_rgb(20, 28, 48)
    } else {
        Color32::from_white_alpha(230)
    }
}

/// A plus: add an account.
pub(crate) fn plus(painter: &egui::Painter, center: Pos2, size: f32, color: Color32) {
    let stroke = Stroke::new(1.75, color);
    let half = size * 0.32;
    painter.line_segment(
        [center - Vec2::new(half, 0.0), center + Vec2::new(half, 0.0)],
        stroke,
    );
    painter.line_segment(
        [center - Vec2::new(0.0, half), center + Vec2::new(0.0, half)],
        stroke,
    );
}

/// A gear drawn as a ring with eight teeth: settings.
pub(crate) fn gear(painter: &egui::Painter, center: Pos2, size: f32, color: Color32) {
    let stroke = Stroke::new(1.5, color);
    let unit = size / 16.0;
    painter.circle_stroke(center, 3.2 * unit, stroke);
    for step in 0..8 {
        let angle = step as f32 * std::f32::consts::FRAC_PI_4;
        let dir = Vec2::angled(angle);
        painter.line_segment(
            [center + dir * 5.6 * unit, center + dir * 7.4 * unit],
            stroke,
        );
    }
    painter.circle_stroke(center, 5.6 * unit, stroke);
}

/// A magnifying glass: search.
pub(crate) fn search(painter: &egui::Painter, center: Pos2, size: f32, color: Color32) {
    let stroke = Stroke::new(1.5, color);
    let unit = size / 14.0;
    let lens = at(center, unit, -1.5, -1.5);
    painter.circle_stroke(lens, 4.6 * unit, stroke);
    painter.line_segment([at(lens, unit, 3.4, 3.4), at(lens, unit, 6.6, 6.6)], stroke);
}

/// An arrow that points up: send.
pub(crate) fn arrow_up(painter: &egui::Painter, center: Pos2, size: f32, color: Color32) {
    let stroke = Stroke::new(2.0, color);
    let unit = size / 16.0;
    painter.line_segment(
        [at(center, unit, 0.0, 6.0), at(center, unit, 0.0, -6.0)],
        stroke,
    );
    painter.line_segment(
        [at(center, unit, -5.5, -0.5), at(center, unit, 0.0, -6.0)],
        stroke,
    );
    painter.line_segment(
        [at(center, unit, 5.5, -0.5), at(center, unit, 0.0, -6.0)],
        stroke,
    );
}

/// A chevron that points left: back to the list above.
pub(crate) fn chevron_left(painter: &egui::Painter, center: Pos2, size: f32, color: Color32) {
    let stroke = Stroke::new(2.0, color);
    let unit = size / 16.0;
    painter.line_segment(
        [at(center, unit, 2.5, -6.0), at(center, unit, -3.5, 0.0)],
        stroke,
    );
    painter.line_segment(
        [at(center, unit, -3.5, 0.0), at(center, unit, 2.5, 6.0)],
        stroke,
    );
}

/// A chevron that points right: open the column on the left again.
pub(crate) fn chevron_right(painter: &egui::Painter, center: Pos2, size: f32, color: Color32) {
    let stroke = Stroke::new(2.0, color);
    let unit = size / 16.0;
    painter.line_segment(
        [at(center, unit, -2.5, -6.0), at(center, unit, 3.5, 0.0)],
        stroke,
    );
    painter.line_segment(
        [at(center, unit, 3.5, 0.0), at(center, unit, -2.5, 6.0)],
        stroke,
    );
}

/// A hash sign: one topic of a forum.
pub(crate) fn hash(painter: &egui::Painter, center: Pos2, size: f32, color: Color32) {
    let stroke = Stroke::new(1.6, color);
    let unit = size / 16.0;
    for x in [-2.0, 2.5] {
        painter.line_segment(
            [
                at(center, unit, x + 1.0, -6.0),
                at(center, unit, x - 1.0, 6.0),
            ],
            stroke,
        );
    }
    for y in [-2.5, 2.5] {
        painter.line_segment(
            [at(center, unit, -6.0, y), at(center, unit, 6.0, y)],
            stroke,
        );
    }
}

/// A speech bubble with two text lines: the empty chat state.
pub(crate) fn chat_bubble(painter: &egui::Painter, center: Pos2, size: f32, color: Color32) {
    let stroke = Stroke::new(2.0, color);
    let unit = size / 48.0;
    let body =
        egui::Rect::from_center_size(at(center, unit, 0.0, -2.0), Vec2::new(40.0, 30.0) * unit);
    painter.rect_stroke(
        body,
        egui::CornerRadius::same((10.0 * unit).round().clamp(2.0, 255.0) as u8),
        stroke,
        egui::StrokeKind::Inside,
    );
    painter.add(egui::Shape::line(
        vec![
            at(center, unit, -8.0, 13.0),
            at(center, unit, -12.0, 21.0),
            at(center, unit, -3.0, 13.0),
        ],
        stroke,
    ));
    for (y, half) in [(-8.0, 10.0), (0.0, 6.0)] {
        painter.line_segment(
            [at(center, unit, -half, y), at(center, unit, half, y)],
            stroke,
        );
    }
}

fn bell_body(painter: &egui::Painter, center: Pos2, unit: f32, stroke: Stroke) {
    painter.add(egui::Shape::line(
        vec![
            at(center, unit, -5.0, 4.0),
            at(center, unit, -4.0, 3.0),
            at(center, unit, -4.0, -1.5),
            at(center, unit, -2.5, -4.5),
            at(center, unit, 0.0, -5.5),
            at(center, unit, 2.5, -4.5),
            at(center, unit, 4.0, -1.5),
            at(center, unit, 4.0, 3.0),
            at(center, unit, 5.0, 4.0),
            at(center, unit, -5.0, 4.0),
        ],
        stroke,
    ));
    painter.line_segment(
        [at(center, unit, -1.6, 6.8), at(center, unit, 1.6, 6.8)],
        stroke,
    );
}

/// A bell: the chat makes noise.
pub(crate) fn bell(painter: &egui::Painter, center: Pos2, size: f32, color: Color32) {
    bell_body(painter, center, size / 16.0, Stroke::new(1.5, color));
}

/// A bell with a slash: the chat is muted.
pub(crate) fn bell_off(painter: &egui::Painter, center: Pos2, size: f32, color: Color32) {
    let stroke = Stroke::new(1.5, color);
    let unit = size / 16.0;
    bell_body(painter, center, unit, stroke);
    painter.line_segment(
        [at(center, unit, -6.5, -6.5), at(center, unit, 6.5, 6.5)],
        stroke,
    );
}

/// The mark of a protocol, for a tile or a badge. Filled shapes so a 12 px
/// badge still reads.
pub(crate) fn protocol(
    painter: &egui::Painter,
    center: Pos2,
    size: f32,
    protocol: ProtocolId,
    color: Color32,
) {
    // Every account of a protocol has the same mark (ADR 0014).
    match protocol.kind() {
        ProtocolKind::Telegram => telegram(painter, center, size, color),
        ProtocolKind::WhatsApp => whatsapp(painter, center, size, color),
        ProtocolKind::Signal => signal(painter, center, size, color),
        ProtocolKind::Discord => discord(painter, center, size, color),
        ProtocolKind::Slack => slack(painter, center, size, color),
    }
}

/// A paper plane that points up-right: Telegram-like.
///
/// One body triangle plus an opaque fold triangle. Split fills with
/// feathered edges left hairline seams, and `gamma_multiply` also scaled
/// alpha so the fold blended with the tile behind it.
fn telegram(painter: &egui::Painter, center: Pos2, size: f32, color: Color32) {
    let unit = size / 16.0;
    let tip = at(center, unit, 6.0, -5.0);
    let left = at(center, unit, -6.0, 0.5);
    let bottom = at(center, unit, -2.0, 6.0);
    let fold = at(center, unit, 0.5, 1.0);
    painter.add(Shape::convex_polygon(
        vec![left, tip, bottom],
        color,
        Stroke::NONE,
    ));
    painter.add(Shape::convex_polygon(
        vec![fold, tip, bottom],
        darken(color, 0.78),
        Stroke::NONE,
    ));
}

/// Keep alpha; scale only the RGB channels.
fn darken(color: Color32, factor: f32) -> Color32 {
    let scale = |channel: u8| (f32::from(channel) * factor).round().clamp(0.0, 255.0) as u8;
    Color32::from_rgba_unmultiplied(
        scale(color.r()),
        scale(color.g()),
        scale(color.b()),
        color.a(),
    )
}

/// A round bubble with a phone handset: WhatsApp-like.
fn whatsapp(painter: &egui::Painter, center: Pos2, size: f32, color: Color32) {
    let unit = size / 16.0;
    painter.circle_filled(at(center, unit, -0.5, -0.8), 6.2 * unit, color);
    painter.add(Shape::convex_polygon(
        vec![
            at(center, unit, -4.5, 3.5),
            at(center, unit, -7.0, 7.0),
            at(center, unit, -1.5, 5.0),
        ],
        color,
        Stroke::NONE,
    ));
    // Keep the stroke thick enough that a ~9 px badge still shows a handset.
    let stroke = Stroke::new((size * 0.16).clamp(1.5, 2.5), cut_on(color));
    painter.add(Shape::line(
        vec![
            at(center, unit, -3.0, -2.4),
            at(center, unit, -3.8, -0.6),
            at(center, unit, -3.0, 1.0),
            at(center, unit, -1.2, 2.4),
            at(center, unit, 0.8, 2.6),
            at(center, unit, 2.2, 1.2),
            at(center, unit, 2.6, -0.4),
        ],
        stroke,
    ));
}

/// A speech bubble: Signal-like.
fn signal(painter: &egui::Painter, center: Pos2, size: f32, color: Color32) {
    let unit = size / 16.0;
    let body =
        egui::Rect::from_center_size(at(center, unit, 0.0, -1.0), Vec2::new(12.0, 10.0) * unit);
    painter.rect_filled(
        body,
        egui::CornerRadius::same((4.5 * unit).round().clamp(2.0, 255.0) as u8),
        color,
    );
    painter.add(Shape::convex_polygon(
        vec![
            at(center, unit, -2.5, 3.5),
            at(center, unit, -5.5, 7.0),
            at(center, unit, 0.5, 4.0),
        ],
        color,
        Stroke::NONE,
    ));
}

/// A soft face with two eyes: Discord-like without the brand asset.
///
/// Body and legs are separate convex fills. A single concave outline spilled
/// across the chin notch under `Shape::convex_polygon`.
fn discord(painter: &egui::Painter, center: Pos2, size: f32, color: Color32) {
    let unit = size / 16.0;
    let body =
        egui::Rect::from_center_size(at(center, unit, 0.0, -0.5), Vec2::new(13.0, 10.0) * unit);
    painter.rect_filled(
        body,
        egui::CornerRadius::same((5.0 * unit).round().clamp(2.0, 255.0) as u8),
        color,
    );
    painter.add(Shape::convex_polygon(
        vec![
            at(center, unit, -5.0, 3.5),
            at(center, unit, -3.2, 6.2),
            at(center, unit, -1.2, 3.5),
        ],
        color,
        Stroke::NONE,
    ));
    painter.add(Shape::convex_polygon(
        vec![
            at(center, unit, 1.2, 3.5),
            at(center, unit, 3.2, 6.2),
            at(center, unit, 5.0, 3.5),
        ],
        color,
        Stroke::NONE,
    ));
    let eye = cut_on(color);
    painter.circle_filled(at(center, unit, -2.4, -0.5), 1.35 * unit, eye);
    painter.circle_filled(at(center, unit, 2.4, -0.5), 1.35 * unit, eye);
}

/// Four rounded bars in a hash: Slack-like.
fn slack(painter: &egui::Painter, center: Pos2, size: f32, color: Color32) {
    let unit = size / 16.0;
    let corner = (1.4 * unit).round().clamp(1.0, 255.0) as u8;
    let bar = |painter: &egui::Painter, x: f32, y: f32, w: f32, h: f32| {
        painter.rect_filled(
            egui::Rect::from_center_size(at(center, unit, x, y), Vec2::new(w, h) * unit),
            egui::CornerRadius::same(corner),
            color,
        );
    };
    bar(painter, -2.6, -3.4, 3.0, 7.5);
    bar(painter, 2.6, 3.4, 3.0, 7.5);
    bar(painter, -3.4, 2.6, 7.5, 3.0);
    bar(painter, 3.4, -2.6, 7.5, 3.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_protocol_mark_paints() {
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(64.0, 64.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let center = ui.max_rect().center();
                for id in ProtocolId::ALL {
                    protocol(ui.painter(), center, 24.0, id, Color32::WHITE);
                    protocol(ui.painter(), center, 12.0, id, Color32::BLACK);
                }
            });
        });
        output.textures_delta.clear();
    }
}
