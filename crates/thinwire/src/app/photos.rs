//! Chat photos. The box size is fixed before the file arrives, so a row does
//! not jump. Decode and blur run off the UI thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use eframe::egui::{self, Color32, Sense, Vec2};
use thinwire_core::Intent;
use thinwire_protocol::{MessagePhoto, PhotoSize, ProtocolId};

use super::motion;

/// Longest side of a chat photo, in points.
const MAX_SIDE: f32 = 360.0;
/// Shortest side when the photo can grow without passing [`MAX_SIDE`].
const MIN_SHORT: f32 = 64.0;

/// The box for a photo. Fit in 360×360. Grow a small photo until the short
/// side is 64, unless that would pass 360. A wide photo stays inside 360
/// even when the short side is under 64.
#[must_use]
pub(crate) fn photo_box(width: u32, height: u32) -> Vec2 {
    let width = width.max(1) as f32;
    let height = height.max(1) as f32;
    let fit = (MAX_SIDE / width).min(MAX_SIDE / height).min(1.0);
    let mut width = width * fit;
    let mut height = height * fit;
    let short = width.min(height);
    if short > 0.0 && short < MIN_SHORT {
        let grow = MIN_SHORT / short;
        if width * grow <= MAX_SIDE && height * grow <= MAX_SIDE {
            width *= grow;
            height *= grow;
        }
    }
    Vec2::new(width, height)
}

/// The size whose width is nearest `2 × display_width`. A tie keeps the larger.
#[must_use]
pub(crate) fn pick_size(sizes: &[PhotoSize], display_width: f32) -> Option<usize> {
    let target = display_width * 2.0;
    sizes
        .iter()
        .enumerate()
        .min_by(|(_, left), (_, right)| {
            let left_delta = (left.width as f32 - target).abs();
            let right_delta = (right.width as f32 - target).abs();
            left_delta
                .partial_cmp(&right_delta)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(right.width.cmp(&left.width))
        })
        .map(|(index, _)| index)
}

enum Load {
    Wait,
    Ready(Vec<u8>),
    Missing,
}

/// One image read or blur. The UI thread only locks this to copy the result.
#[derive(Clone)]
struct BytesSlot {
    load: Arc<Mutex<Load>>,
    done: Arc<AtomicBool>,
}

impl BytesSlot {
    fn wait() -> Self {
        Self {
            load: Arc::new(Mutex::new(Load::Wait)),
            done: Arc::new(AtomicBool::new(false)),
        }
    }

    fn current(&self) -> Option<Vec<u8>> {
        match &*self.lock() {
            Load::Ready(bytes) => Some(bytes.clone()),
            Load::Wait | Load::Missing => None,
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn settled(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    fn finish(&self, bytes: Option<Vec<u8>>) {
        *self.lock() = match bytes {
            Some(bytes) if !bytes.is_empty() => Load::Ready(bytes),
            _ => Load::Missing,
        };
        self.done.store(true, Ordering::Release);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Load> {
        self.load.lock().unwrap_or_else(|error| error.into_inner())
    }
}

#[derive(Clone, Default)]
struct PhotoUi {
    revealed: Vec<String>,
    overlay: Option<String>,
    requested: Vec<(String, i32)>,
    /// Reads and blurs started for this screen. A snapshot waits until they end.
    loads: Vec<BytesSlot>,
}

/// `true` when every photo job has finished and no image is still decoding.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn visuals_ready(ctx: &egui::Context) -> bool {
    photo_ui(ctx).loads.iter().all(BytesSlot::settled) && !ctx.has_pending_images()
}

/// Close the full photo. `true` when it was open.
pub(crate) fn take_escape(ctx: &egui::Context) -> bool {
    let mut closed = false;
    ctx.data_mut(|data| {
        let mut ui = data.get_temp::<PhotoUi>(ui_id()).unwrap_or_default();
        closed = ui.overlay.take().is_some();
        data.insert_temp(ui_id(), ui);
    });
    closed
}

pub(crate) fn show(
    ui: &mut egui::Ui,
    protocol: ProtocolId,
    conversation_id: &str,
    message_id: &str,
    photo: &MessagePhoto,
    out: &mut Vec<Intent>,
) {
    let size = photo_box(photo.width, photo.height);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    ui.ctx().data_mut(|data| {
        data.insert_temp(egui::Id::new("thinwire-photo-box-height"), rect.height());
    });
    let mut shown = is_revealed(ui.ctx(), message_id);
    if response.clicked() {
        if photo.has_spoiler && !shown {
            reveal(ui.ctx(), message_id);
            shown = true;
        } else if !photo.is_secret && full_bytes(ui, message_id, photo).is_some() {
            open_overlay(ui.ctx(), message_id);
        }
    }
    let spoiler_hidden = photo.has_spoiler && !shown;
    paint(ui, rect, message_id, photo, spoiler_hidden);
    if photo.failed {
        forget_request(ui.ctx(), message_id);
        failure(ui, rect, protocol, conversation_id, message_id, photo, out);
        return;
    }
    if spoiler_hidden {
        return;
    }
    maybe_download(ui, rect, protocol, conversation_id, message_id, photo, out);
    if !full_ready(ui, message_id, photo) {
        // `place` paints in the photo box and leaves the row height alone.
        let _ = ui.place(rect, |ui: &mut egui::Ui| {
            ui.set_clip_rect(rect);
            ui.centered_and_justified(motion::spinner).response
        });
    }
}

fn paint(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    message_id: &str,
    photo: &MessagePhoto,
    spoiler_hidden: bool,
) {
    ui.painter()
        .rect_filled(rect, 4.0, Color32::from_black_alpha(24));
    if !spoiler_hidden && let Some(bytes) = full_bytes(ui, message_id, photo) {
        paint_bytes(
            ui,
            &format!("bytes://thinwire/photo/{message_id}"),
            &bytes,
            rect,
        );
        return;
    }
    if let Some(png) = blurred_mini(ui, message_id, photo) {
        paint_bytes(
            ui,
            &format!("bytes://thinwire/mini/{message_id}"),
            &png,
            rect,
        );
    }
}

fn failure(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    protocol: ProtocolId,
    conversation_id: &str,
    message_id: &str,
    photo: &MessagePhoto,
    out: &mut Vec<Intent>,
) {
    ui.painter().text(
        rect.center() - egui::vec2(0.0, 12.0),
        egui::Align2::CENTER_CENTER,
        "Image didn't load.",
        egui::FontId::proportional(13.0),
        super::theme::palette(ui).text,
    );
    let button = egui::Rect::from_center_size(
        rect.center() + egui::vec2(0.0, 14.0),
        egui::vec2(72.0, 22.0),
    );
    let response = ui.place(button, egui::Button::new("Retry").small());
    if response.clicked()
        && let Some(file_id) = download_id(photo, rect.width())
    {
        out.push(Intent::LoadPhoto {
            protocol,
            conversation_id: conversation_id.to_owned(),
            message_id: message_id.to_owned(),
            file_id,
        });
    }
}

fn maybe_download(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    protocol: ProtocolId,
    conversation_id: &str,
    message_id: &str,
    photo: &MessagePhoto,
    out: &mut Vec<Intent>,
) {
    if !ui.is_rect_visible(rect) || full_ready(ui, message_id, photo) {
        return;
    }
    let Some(file_id) = download_id(photo, rect.width()) else {
        return;
    };
    let key = (message_id.to_owned(), file_id);
    let mut state = photo_ui(ui.ctx());
    if state.requested.contains(&key) {
        return;
    }
    state.requested.push(key);
    store(ui.ctx(), state);
    out.push(Intent::LoadPhoto {
        protocol,
        conversation_id: conversation_id.to_owned(),
        message_id: message_id.to_owned(),
        file_id,
    });
}

fn download_id(photo: &MessagePhoto, display_width: f32) -> Option<i32> {
    let index = pick_size(&photo.sizes, display_width)?;
    let size = photo.sizes.get(index)?;
    if size.ready() {
        return None;
    }
    size.file_id
}

fn full_ready(ui: &egui::Ui, message_id: &str, photo: &MessagePhoto) -> bool {
    full_bytes(ui, message_id, photo).is_some()
}

fn full_bytes(ui: &egui::Ui, message_id: &str, photo: &MessagePhoto) -> Option<Vec<u8>> {
    for size in &photo.sizes {
        if let Some(bytes) = &size.bytes {
            return Some(bytes.as_slice().to_vec());
        }
    }
    for size in &photo.sizes {
        let Some(path) = &size.path else {
            continue;
        };
        let Some(file_id) = size.file_id else {
            continue;
        };
        if let Some(bytes) = read_path(ui, message_id, file_id, path.as_str()) {
            return Some(bytes);
        }
    }
    None
}

fn read_path(ui: &egui::Ui, message_id: &str, file_id: i32, path: &str) -> Option<Vec<u8>> {
    let id = egui::Id::new(("thinwire-photo-read", message_id, file_id));
    if let Some(slot) = ui.data(|data| data.get_temp::<BytesSlot>(id)) {
        return slot.current();
    }
    let slot = BytesSlot::wait();
    track(ui.ctx(), slot.clone());
    let worker = slot.clone();
    let ctx = ui.ctx().clone();
    let path = path.to_owned();
    std::thread::spawn(move || {
        let bytes = std::fs::read(&path).ok();
        worker.finish(bytes);
        ctx.request_repaint();
    });
    ui.data_mut(|data| data.insert_temp(id, slot));
    None
}

fn blurred_mini(ui: &egui::Ui, message_id: &str, photo: &MessagePhoto) -> Option<Vec<u8>> {
    let mini = photo.mini.as_ref()?;
    let id = egui::Id::new(("thinwire-photo-blur", message_id));
    if let Some(slot) = ui.data(|data| data.get_temp::<BytesSlot>(id)) {
        return slot.current();
    }
    let slot = BytesSlot::wait();
    track(ui.ctx(), slot.clone());
    let worker = slot.clone();
    let ctx = ui.ctx().clone();
    let bytes = mini.as_slice().to_vec();
    std::thread::spawn(move || {
        let png = blur_png(&bytes);
        worker.finish(png);
        ctx.request_repaint();
    });
    ui.data_mut(|data| data.insert_temp(id, slot));
    None
}

fn blur_png(bytes: &[u8]) -> Option<Vec<u8>> {
    let image = image::load_from_memory(bytes).ok()?.to_rgba8();
    let blurred = image::imageops::blur(&image, 8.0);
    let mut png = Vec::new();
    blurred
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .ok()?;
    Some(png)
}

fn paint_bytes(ui: &mut egui::Ui, uri: &str, bytes: &[u8], rect: egui::Rect) {
    let id = egui::Id::new(("thinwire-photo-included", uri));
    let included = ui.data(|data| data.get_temp::<bool>(id).unwrap_or(false));
    if !included {
        ui.ctx().include_bytes(uri.to_owned(), bytes.to_vec());
        ui.data_mut(|data| data.insert_temp(id, true));
    }
    egui::Image::from_uri(uri)
        .fit_to_exact_size(rect.size())
        .show_loading_spinner(false)
        .paint_at(ui, rect);
}

/// The full photo, fitted in the window. Esc and Close leave it.
pub(crate) fn overlay(ui: &mut egui::Ui, messages: &[thinwire_protocol::ChatMessage]) {
    let Some(message_id) = photo_ui(ui.ctx()).overlay.clone() else {
        return;
    };
    let Some(message) = messages.iter().find(|message| message.id == message_id) else {
        return;
    };
    let Some(photo) = message.photo.as_ref() else {
        return;
    };
    if photo.is_secret {
        return;
    }
    let Some(bytes) = full_bytes(ui, &message.id, photo) else {
        return;
    };
    let screen = ui.ctx().content_rect();
    egui::Area::new(egui::Id::new("photo-overlay"))
        .order(egui::Order::Foreground)
        .fixed_pos(screen.min)
        .show(ui.ctx(), |ui| {
            let _ = ui.allocate_rect(screen, Sense::click());
            ui.painter()
                .rect_filled(screen, 0.0, Color32::from_black_alpha(180));
            let margin = 48.0;
            let fitted = fit_in(
                photo_box(photo.width, photo.height),
                screen.shrink(margin).size(),
            );
            let rect = egui::Rect::from_center_size(screen.center(), fitted);
            paint_bytes(
                ui,
                &format!("bytes://thinwire/overlay/{}", message.id),
                &bytes,
                rect,
            );
            let close = egui::Rect::from_min_size(
                screen.right_top() + egui::vec2(-88.0, 16.0),
                egui::vec2(72.0, 28.0),
            );
            if ui.put(close, egui::Button::new("Close")).clicked() {
                take_escape(ui.ctx());
            }
        });
}

fn fit_in(size: Vec2, bounds: Vec2) -> Vec2 {
    let scale = (bounds.x / size.x).min(bounds.y / size.y).min(1.0);
    size * scale.max(0.0)
}

fn reveal(ctx: &egui::Context, message_id: &str) {
    let mut state = photo_ui(ctx);
    if !state.revealed.iter().any(|id| id == message_id) {
        state.revealed.push(message_id.to_owned());
    }
    store(ctx, state);
}

fn is_revealed(ctx: &egui::Context, message_id: &str) -> bool {
    photo_ui(ctx).revealed.iter().any(|id| id == message_id)
}

fn track(ctx: &egui::Context, slot: BytesSlot) {
    let mut state = photo_ui(ctx);
    state.loads.push(slot);
    store(ctx, state);
}

fn open_overlay(ctx: &egui::Context, message_id: &str) {
    let mut state = photo_ui(ctx);
    state.overlay = Some(message_id.to_owned());
    store(ctx, state);
}

fn forget_request(ctx: &egui::Context, message_id: &str) {
    let mut state = photo_ui(ctx);
    let before = state.requested.len();
    state.requested.retain(|(id, _)| id != message_id);
    if state.requested.len() != before {
        store(ctx, state);
    }
}

fn photo_ui(ctx: &egui::Context) -> PhotoUi {
    ctx.data(|data| data.get_temp(ui_id()).unwrap_or_default())
}

fn store(ctx: &egui::Context, state: PhotoUi) {
    ctx.data_mut(|data| data.insert_temp(ui_id(), state));
}

fn ui_id() -> egui::Id {
    egui::Id::new("thinwire-photo-ui")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_photo_box_does_not_change_when_the_bytes_arrive() {
        let before = photo_box(360, 144);
        let after = photo_box(360, 144);
        assert_eq!(before, after);
        let tall = photo_box(144, 360);
        assert!((tall.x - 144.0).abs() < 0.1 && (tall.y - 360.0).abs() < 0.1);
        let small = photo_box(40, 40);
        assert!((small.x - 64.0).abs() < 0.1 && (small.y - 64.0).abs() < 0.1);
        let wide = photo_box(2000, 100);
        assert!(wide.x <= MAX_SIDE && wide.y <= MAX_SIDE);
        assert!(wide.y < MIN_SHORT);
    }

    #[test]
    fn the_download_picks_about_twice_the_display_width() {
        let sizes = [
            PhotoSize {
                width: 80,
                height: 40,
                file_id: Some(1),
                path: None,
                bytes: None,
            },
            PhotoSize {
                width: 320,
                height: 160,
                file_id: Some(2),
                path: None,
                bytes: None,
            },
            PhotoSize {
                width: 640,
                height: 320,
                file_id: Some(3),
                path: None,
                bytes: None,
            },
        ];
        // Display width 180 wants about 360. 320 is closer than 640.
        assert_eq!(pick_size(&sizes, 180.0), Some(1));
        // A tie at 480 keeps the larger size.
        assert_eq!(pick_size(&sizes, 240.0), Some(2));
    }

    #[test]
    fn a_loading_photo_keeps_the_row_height_of_a_loaded_photo() {
        let loaded = row_height(sample_photo(true, false));
        let loading = row_height(sample_photo(false, false));
        let failed = row_height(sample_photo(false, true));
        assert_eq!(loading, loaded, "loading {loading} loaded {loaded}");
        assert_eq!(failed, loaded, "failed {failed} loaded {loaded}");
        assert!(loaded > 140.0, "{loaded}");
    }

    fn sample_photo(loaded: bool, failed: bool) -> MessagePhoto {
        MessagePhoto {
            width: 360,
            height: 144,
            mini: None,
            sizes: vec![PhotoSize {
                width: 360,
                height: 144,
                file_id: Some(1),
                path: None,
                bytes: loaded.then(|| thinwire_protocol::ImageBytes::new(vec![1, 2, 3])),
            }],
            has_spoiler: false,
            is_secret: false,
            failed,
        }
    }

    fn row_height(photo: MessagePhoto) -> f32 {
        let ctx = egui::Context::default();
        super::super::theme::install(&ctx);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900.0, 700.0),
            )),
            ..Default::default()
        };
        let mut height = 0.0;
        let mut output = ctx.run_ui(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let top = ui.cursor().min.y;
                show(
                    ui,
                    ProtocolId::Telegram,
                    "telegram:1",
                    "telegram:1:2",
                    &photo,
                    &mut Vec::new(),
                );
                ui.label("Hello");
                height = ui.cursor().min.y - top;
            });
        });
        // The image loader queues a texture. This test only checks layout.
        output.textures_delta.clear();
        let _ = visuals_ready(&ctx);
        height
    }

    #[test]
    fn the_photo_spinner_goes_through_motion() {
        let src = include_str!("photos.rs");
        assert!(src.contains("motion::spinner"));
        assert!(!src.contains(concat!("ui.spin", "ner")));
        assert!(!src.contains(concat!("Spin", "ner::")));
    }
}
