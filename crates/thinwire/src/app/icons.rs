//! Icons the shell draws itself. Inter and the egui fallback fonts miss several
//! symbol glyphs, so each mark is painted here. Every icon fits a square of
//! `size` centered on `center`. Stroke icons and filled protocol marks share
//! this module.
//!
//! Protocol marks are original drawings that only resemble the common shapes.
//! They are not the brand assets.

use eframe::egui::{self, Color32, Pos2, Shape, Stroke, Vec2};
use thinwire_protocol::{ProtocolId, ProtocolKind};

use super::motion::mix;

fn at(center: Pos2, unit: f32, x: f32, y: f32) -> Pos2 {
    center + Vec2::new(x, y) * unit
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
/// badge still reads. `color` is the mark and `cut` is the color behind it:
/// a detail inside the mark has the `cut` color, so it looks like a hole.
pub(crate) fn protocol(
    painter: &egui::Painter,
    center: Pos2,
    size: f32,
    protocol: ProtocolId,
    color: Color32,
    cut: Color32,
) {
    // Every account of a protocol has the same mark (ADR 0014).
    match protocol.kind() {
        ProtocolKind::Telegram => telegram(painter, center, size, color, cut),
        ProtocolKind::WhatsApp => whatsapp(painter, center, size, color, cut),
        ProtocolKind::Signal => signal(painter, center, size, color, cut),
        ProtocolKind::Discord => discord(painter, center, size, color, cut),
        ProtocolKind::Slack => slack(painter, center, size, color),
    }
}

/// A rounded square with a top-to-bottom gradient: the tile behind a
/// protocol mark.
///
/// A mesh has no feathered edge. So a flat rounded square in the middle
/// color paints first, and the gradient mesh sits half a pixel inside it.
pub(crate) fn gradient_tile(
    painter: &egui::Painter,
    rect: egui::Rect,
    corner: f32,
    top: Color32,
    bottom: Color32,
) {
    painter.rect_filled(
        rect,
        egui::CornerRadius::same(corner.round().clamp(0.0, 255.0) as u8),
        mix(top, bottom, 0.5),
    );
    let inner = rect.shrink(0.5);
    let radius = (corner - 0.5).max(0.0);
    let mut mesh = egui::Mesh::default();
    let shade = |pos: Pos2| mix(top, bottom, (pos.y - inner.top()) / inner.height());
    mesh.colored_vertex(inner.center(), shade(inner.center()));
    let corners = [
        (inner.right_bottom() - Vec2::splat(radius), 0.0),
        (inner.left_bottom() + Vec2::new(radius, -radius), 90.0),
        (inner.left_top() + Vec2::splat(radius), 180.0),
        (inner.right_top() + Vec2::new(-radius, radius), 270.0),
    ];
    for (center, start) in corners {
        for pos in arc(center, radius, start, start + 90.0, CORNER_STEPS) {
            mesh.colored_vertex(pos, shade(pos));
        }
    }
    let rim = mesh.vertices.len() as u32 - 1;
    for index in 0..rim {
        mesh.add_triangle(0, 1 + index, 1 + (index + 1) % rim);
    }
    painter.add(Shape::mesh(mesh));
}

/// Line segments in each corner of a gradient tile.
const CORNER_STEPS: usize = 8;

/// Points on a circle from `from` to `to` degrees. 0 points right and 90
/// points down, because y grows down on the screen.
fn arc(center: Pos2, radius: f32, from: f32, to: f32, steps: usize) -> Vec<Pos2> {
    (0..=steps)
        .map(|step| {
            let angle = egui::lerp(from..=to, step as f32 / steps as f32).to_radians();
            center + Vec2::new(angle.cos(), angle.sin()) * radius
        })
        .collect()
}

/// A paper plane that points up-right: Telegram-like.
///
/// A wing, a body under it, and a small tail fin. The parts do not overlap
/// much, and each one is opaque: a part with alpha showed a seam where two
/// feathered edges met.
fn telegram(painter: &egui::Painter, center: Pos2, size: f32, color: Color32, cut: Color32) {
    let unit = size / 16.0;
    let nose = at(center, unit, 6.6, -5.6);
    let wing = at(center, unit, -6.8, -0.6);
    let fold = at(center, unit, -1.6, 1.7);
    let keel = at(center, unit, 2.0, 6.2);
    let fin = at(center, unit, -1.2, 5.6);
    let shade = mix(color, cut, 0.3);
    painter.add(Shape::convex_polygon(
        vec![fold, at(center, unit, 0.4, 3.6), fin],
        mix(color, cut, 0.5),
        Stroke::NONE,
    ));
    painter.add(Shape::convex_polygon(
        vec![fold, nose, keel],
        shade,
        Stroke::NONE,
    ));
    painter.add(Shape::convex_polygon(
        vec![wing, nose, fold],
        color,
        Stroke::NONE,
    ));
}

/// A square speech bubble with a phone handset: WhatsApp-like. The square
/// body and the tail on the right keep it apart from the brand mark.
fn whatsapp(painter: &egui::Painter, center: Pos2, size: f32, color: Color32, cut: Color32) {
    let unit = size / 16.0;
    let body =
        egui::Rect::from_center_size(at(center, unit, 0.0, -0.9), Vec2::new(13.0, 11.4) * unit);
    painter.rect_filled(
        body,
        egui::CornerRadius::same((4.2 * unit).round().clamp(1.0, 255.0) as u8),
        color,
    );
    painter.add(Shape::convex_polygon(
        vec![
            at(center, unit, 1.6, 4.2),
            at(center, unit, 6.0, 7.2),
            at(center, unit, 5.6, 1.6),
        ],
        color,
        Stroke::NONE,
    ));
    // The handset is one arc with a round end on each side. Keep it thick
    // enough that a small badge still shows it.
    let width = (1.4 * unit).max(1.1);
    let handset = arc(at(center, unit, 1.3, -2.3), 3.8 * unit, 83.0, 186.0, 12);
    let ends = [handset[0], handset[handset.len() - 1]];
    painter.add(Shape::line(handset, Stroke::new(width, cut)));
    for end in ends {
        painter.circle_filled(end, width * 0.85, cut);
    }
}

/// Under this size a mark has no fine detail: a badge on an avatar.
const DETAIL_MIN: f32 = 12.0;

/// A round speech bubble with a ring in it: Signal-like.
fn signal(painter: &egui::Painter, center: Pos2, size: f32, color: Color32, cut: Color32) {
    let unit = size / 16.0;
    let middle = at(center, unit, 0.3, -0.6);
    painter.circle_filled(middle, 6.5 * unit, color);
    painter.add(Shape::convex_polygon(
        vec![
            at(center, unit, -5.6, 2.2),
            at(center, unit, -6.8, 6.9),
            at(center, unit, -2.0, 5.5),
        ],
        color,
        Stroke::NONE,
    ));
    if size >= DETAIL_MIN {
        painter.circle_stroke(middle, 4.0 * unit, Stroke::new(1.1 * unit, cut));
    }
}

/// A game controller with two round buttons: Discord-like without the brand
/// asset.
///
/// The body and the two grips are separate convex fills.
fn discord(painter: &egui::Painter, center: Pos2, size: f32, color: Color32, cut: Color32) {
    let unit = size / 16.0;
    let body =
        egui::Rect::from_center_size(at(center, unit, 0.0, -0.9), Vec2::new(13.6, 8.0) * unit);
    painter.rect_filled(
        body,
        egui::CornerRadius::same((3.6 * unit).round().clamp(1.0, 255.0) as u8),
        color,
    );
    for side in [-1.0, 1.0] {
        painter.circle_filled(at(center, unit, 4.2 * side, 2.3), 3.0 * unit, color);
        painter.circle_filled(at(center, unit, 2.7 * side, -0.4), 1.45 * unit, cut);
    }
}

/// Four pills in a pinwheel hash: Slack-like.
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
    bar(painter, -2.5, -3.1, 2.8, 8.0);
    bar(painter, 2.5, 3.1, 2.8, 8.0);
    bar(painter, -3.1, 2.5, 8.0, 2.8);
    bar(painter, 3.1, -2.5, 8.0, 2.8);
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
                    protocol(
                        ui.painter(),
                        center,
                        24.0,
                        id,
                        Color32::WHITE,
                        Color32::BLUE,
                    );
                    protocol(
                        ui.painter(),
                        center,
                        9.0,
                        id,
                        Color32::BLACK,
                        Color32::WHITE,
                    );
                    gradient_tile(
                        ui.painter(),
                        egui::Rect::from_center_size(center, Vec2::splat(36.0)),
                        10.0,
                        Color32::RED,
                        Color32::BLUE,
                    );
                }
            });
        });
        output.textures_delta.clear();
    }
}
