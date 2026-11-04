//! Icons painted with strokes. Inter and the egui fallback fonts miss several
//! symbol glyphs, so the shell draws each icon itself. Every icon fits a square
//! of `size` centered on `center`.

use eframe::egui::{self, Color32, Pos2, Stroke, Vec2};

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
