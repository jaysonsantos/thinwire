//! Chat photos. The box size is fixed before the file arrives, so a row does
//! not jump. Decode and blur run off the UI thread. A finished read drops
//! its bytes once egui has them.

use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};

use eframe::egui::{self, Color32, Sense, Vec2};
use thinwire_core::Intent;
use thinwire_protocol::{ChatAvatar, MessagePhoto, PhotoSize, ProtocolId};

use super::motion;

/// Diameter of an inbox avatar, in points.
pub(crate) const AVATAR_SIDE: f32 = 36.0;
/// Corner radius that turns the 36 px square into a circle.
const AVATAR_RADIUS: u8 = 18;

/// Longest side of a chat photo, in points.
const MAX_SIDE: f32 = 360.0;
/// Shortest side when the photo can grow without passing [`MAX_SIDE`].
const MIN_SHORT: f32 = 64.0;
/// Workers that read and blur photos. The UI thread only queues a job.
const PHOTO_WORKERS: usize = 2;
/// Jobs waiting for a worker. A full queue leaves the photo for a later frame.
const PHOTO_QUEUE: usize = 4;

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
    Ready {
        bytes: Vec<u8>,
        stamp: u64,
    },
    /// egui already has these bytes. The slot keeps the URI only.
    Cached {
        uri: String,
    },
    Missing,
}

enum View {
    Wait,
    Ready { bytes: Vec<u8>, stamp: u64 },
    Cached { uri: String },
    Missing,
}

/// One image read or blur. The UI thread only locks this to copy the result.
#[derive(Clone)]
struct BytesSlot {
    load: Arc<Mutex<Load>>,
    done: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
}

impl BytesSlot {
    fn wait() -> Self {
        Self {
            load: Arc::new(Mutex::new(Load::Wait)),
            done: Arc::new(AtomicBool::new(false)),
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    fn view(&self) -> View {
        match &*self.lock() {
            Load::Wait => View::Wait,
            Load::Ready { bytes, stamp } => View::Ready {
                bytes: bytes.clone(),
                stamp: *stamp,
            },
            Load::Cached { uri } => View::Cached { uri: uri.clone() },
            Load::Missing => View::Missing,
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn settled(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    /// `true` while the job is still running or still holds image bytes.
    fn holds_work(&self) -> bool {
        matches!(*self.lock(), Load::Wait | Load::Ready { .. })
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn holds_bytes(&self) -> bool {
        matches!(*self.lock(), Load::Ready { .. })
    }

    /// The worker finished. A later [`Self::finish`] keeps no bytes.
    fn abandon(&self) {
        self.cancelled.store(true, Ordering::Release);
        *self.lock() = Load::Missing;
        self.done.store(true, Ordering::Release);
    }

    fn abandoned(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Drop the bytes after egui has stored them.
    fn hand_to_egui(&self, uri: String) {
        let mut guard = self.lock();
        if matches!(*guard, Load::Ready { .. }) {
            *guard = Load::Cached { uri };
        }
    }

    fn finish(&self, bytes: Option<Vec<u8>>) {
        let mut guard = self.lock();
        if self.cancelled.load(Ordering::Acquire) {
            *guard = Load::Missing;
            drop(guard);
            self.done.store(true, Ordering::Release);
            return;
        }
        *guard = match bytes {
            Some(bytes) if !bytes.is_empty() => {
                let stamp = fingerprint(&bytes);
                Load::Ready { bytes, stamp }
            }
            _ => Load::Missing,
        };
        drop(guard);
        self.done.store(true, Ordering::Release);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Load> {
        self.load.lock().unwrap_or_else(|error| error.into_inner())
    }
}

/// One image to paint. `fresh` is set only on the frame that gives the bytes
/// to egui.
struct Shown {
    uri: String,
    fresh: Option<Vec<u8>>,
    slot: Option<BytesSlot>,
}

#[derive(Clone, Default)]
struct PhotoUi {
    revealed: Vec<String>,
    overlay: Option<String>,
    /// Reads and blurs that have not handed their bytes to egui yet.
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

/// One inbox avatar. A ready photo uses the chat-photo loader and cache.
/// With no photo, or while the file is still loading, the circle shows initials.
pub(crate) fn paint_avatar(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    protocol: ProtocolId,
    conversation_id: &str,
    title: &str,
    avatar: &ChatAvatar,
    out: &mut Vec<Intent>,
) {
    prune_loads(ui.ctx());
    let ink = super::theme::avatar_ink(avatar.accent, ui.visuals().dark_mode);
    // Opaque circle, so the row selection fill does not show through.
    ui.painter()
        .circle_filled(rect.center(), AVATAR_SIDE * 0.5, ink.fill);
    let shown = avatar_image(ui, protocol, conversation_id, avatar);
    if let Some(shown) = &shown {
        paint_circle(ui, &avatar_base(protocol, conversation_id), shown, rect);
    } else {
        paint_initials(ui, rect, title, ink.text);
    }
    // One ask per file. A repeat each frame would dispatch each frame, and
    // each dispatch asks for a repaint. The core sets `requested` when it
    // queues the download.
    if protocol.downloads_avatar()
        && shown.is_none()
        && !avatar.ready()
        && !avatar.failed
        && !avatar.requested
        && ui.is_rect_visible(rect)
        && let Some(file_id) = avatar.file_id
    {
        out.push(Intent::LoadAvatar {
            protocol,
            conversation_id: conversation_id.to_owned(),
            file_id,
        });
    }
}

/// Up to two initials from `title`. A title with no letters is "?".
#[must_use]
pub(crate) fn initials(title: &str) -> String {
    let mut letters = String::new();
    for word in title.split_whitespace() {
        let Some(ch) = word.chars().find(|ch| ch.is_alphanumeric()) else {
            continue;
        };
        letters.extend(ch.to_uppercase());
        if letters.chars().count() >= 2 {
            break;
        }
    }
    if letters.is_empty() {
        "?".into()
    } else {
        letters
    }
}

fn avatar_image(
    ui: &egui::Ui,
    protocol: ProtocolId,
    conversation_id: &str,
    avatar: &ChatAvatar,
) -> Option<Shown> {
    let base = avatar_base(protocol, conversation_id);
    if let Some(bytes) = &avatar.bytes {
        let slice = bytes.as_slice();
        if !slice.is_empty() {
            return Some(shown_inline(ui, &base, slice));
        }
    }
    let path = avatar.path.as_ref()?.as_str();
    let file_id = avatar.file_id?;
    let slot = format!("{protocol}/{conversation_id}");
    read_path(ui, &slot, &base, file_id, path)
}

fn avatar_base(protocol: ProtocolId, conversation_id: &str) -> String {
    format!("bytes://thinwire/avatar/{protocol}/{conversation_id}")
}

fn paint_circle(ui: &mut egui::Ui, base: &str, shown: &Shown, rect: egui::Rect) {
    if let Some(bytes) = &shown.fresh {
        publish(ui, base, &shown.uri, bytes);
        if let Some(slot) = &shown.slot {
            slot.hand_to_egui(shown.uri.clone());
        }
    }
    // Default image tint is white, which leaves the photo untinted.
    egui::Image::from_uri(&shown.uri)
        .fit_to_exact_size(rect.size())
        .corner_radius(egui::CornerRadius::same(AVATAR_RADIUS))
        .show_loading_spinner(false)
        .paint_at(ui, rect);
}

fn paint_initials(ui: &egui::Ui, rect: egui::Rect, title: &str, color: Color32) {
    let font = egui::FontId::new(super::theme::size::BUTTON, super::theme::semibold());
    let galley = ui.painter().layout_no_wrap(initials(title), font, color);
    let pos = egui::pos2(
        rect.center().x - galley.size().x * 0.5,
        rect.center().y - galley.size().y * 0.5,
    );
    ui.painter().galley(pos, galley, color);
}

pub(crate) fn show(
    ui: &mut egui::Ui,
    protocol: ProtocolId,
    conversation_id: &str,
    message_id: &str,
    photo: &MessagePhoto,
    out: &mut Vec<Intent>,
) {
    prune_loads(ui.ctx());
    let size = photo_box(photo.width, photo.height);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    ui.ctx().data_mut(|data| {
        data.insert_temp(egui::Id::unique("thinwire-photo-box-height"), rect.height());
    });
    let mut revealed_now = is_revealed(ui.ctx(), message_id);
    let spoiler_was_hidden = photo.has_spoiler && !revealed_now;
    if response.clicked() && spoiler_was_hidden {
        reveal(ui.ctx(), message_id);
        revealed_now = true;
    }
    let spoiler_hidden = photo.has_spoiler && !revealed_now;
    let selected = if spoiler_hidden {
        None
    } else {
        selected_image(ui, message_id, photo, rect.width())
    };
    // The click that reveals a spoiler does not also open the full view.
    if response.clicked() && !spoiler_was_hidden && !photo.is_secret && selected.is_some() {
        open_overlay(ui.ctx(), message_id);
    }
    paint(
        ui,
        rect,
        message_id,
        photo,
        spoiler_hidden,
        selected.as_ref(),
    );
    if photo.failed {
        failure(ui, rect, protocol, conversation_id, message_id, photo, out);
        return;
    }
    if spoiler_hidden {
        return;
    }
    maybe_download(ui, rect, protocol, conversation_id, message_id, photo, out);
    if selected.is_none() {
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
    selected: Option<&Shown>,
) {
    ui.painter()
        .rect_filled(rect, 4.0, Color32::from_black_alpha(24));
    if spoiler_hidden {
        paint_mini(ui, rect, message_id, photo);
        return;
    }
    if let Some(shown) = selected {
        paint_photo(ui, message_id, shown, rect);
        return;
    }
    if let Some(shown) = placeholder_image(ui, message_id, photo, rect.width()) {
        paint_photo(ui, message_id, &shown, rect);
        return;
    }
    paint_mini(ui, rect, message_id, photo);
}

fn paint_photo(ui: &mut egui::Ui, message_id: &str, shown: &Shown, rect: egui::Rect) {
    paint_shown(ui, &photo_base(message_id), shown, rect);
    // The blur is only the stand-in. Drop it once a real size is on screen.
    forget_base(ui, &mini_base(message_id));
}

fn paint_mini(ui: &mut egui::Ui, rect: egui::Rect, message_id: &str, photo: &MessagePhoto) {
    if let Some(shown) = blurred_mini(ui, message_id, photo) {
        paint_shown(ui, &mini_base(message_id), &shown, rect);
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
    if !ui.is_rect_visible(rect) {
        return;
    }
    // A cached smaller size is a placeholder. It does not satisfy this box.
    // The core ignores a repeat while that file is already in flight.
    let Some(file_id) = download_id(photo, rect.width()) else {
        return;
    };
    out.push(Intent::LoadPhoto {
        protocol,
        conversation_id: conversation_id.to_owned(),
        message_id: message_id.to_owned(),
        file_id,
    });
}

/// The file of the size for this bubble, when that size is not here yet.
fn download_id(photo: &MessagePhoto, display_width: f32) -> Option<i32> {
    let index = pick_size(&photo.sizes, display_width)?;
    let size = photo.sizes.get(index)?;
    if size.ready() {
        return None;
    }
    size.file_id
}

fn selected_size(photo: &MessagePhoto, display_width: f32) -> Option<&PhotoSize> {
    let index = pick_size(&photo.sizes, display_width)?;
    photo.sizes.get(index)
}

fn selected_image(
    ui: &egui::Ui,
    message_id: &str,
    photo: &MessagePhoto,
    display_width: f32,
) -> Option<Shown> {
    let size = selected_size(photo, display_width)?;
    if !size.ready() {
        return None;
    }
    load_size(ui, message_id, &photo_base(message_id), size)
}

/// The largest cached size other than the one this bubble asked for.
/// A smaller thumbnail can stand in, and it does not cancel that download.
fn placeholder_image(
    ui: &egui::Ui,
    message_id: &str,
    photo: &MessagePhoto,
    display_width: f32,
) -> Option<Shown> {
    let selected = pick_size(&photo.sizes, display_width)?;
    let placeholder = photo
        .sizes
        .iter()
        .enumerate()
        .filter(|(index, size)| *index != selected && size.ready())
        .max_by_key(|(_, size)| size.width)
        .map(|(_, size)| size)?;
    load_size(ui, message_id, &photo_base(message_id), placeholder)
}

fn load_size(ui: &egui::Ui, message_id: &str, base: &str, size: &PhotoSize) -> Option<Shown> {
    if let Some(bytes) = &size.bytes {
        let slice = bytes.as_slice();
        if slice.is_empty() {
            return None;
        }
        return Some(shown_inline(ui, base, slice));
    }
    let path = size.path.as_ref()?.as_str();
    let file_id = size.file_id?;
    read_path(ui, message_id, base, file_id, path)
}

fn shown_inline(ui: &egui::Ui, base: &str, bytes: &[u8]) -> Shown {
    let stamp = fingerprint(bytes);
    let uri = image_uri(base, stamp);
    let known = remembered_uri(ui, base).as_deref() == Some(uri.as_str());
    Shown {
        uri,
        fresh: (!known).then(|| bytes.to_vec()),
        slot: None,
    }
}

fn read_path(
    ui: &egui::Ui,
    message_id: &str,
    base: &str,
    file_id: i32,
    path: &str,
) -> Option<Shown> {
    // The path is not part of the id source: egui keeps that string for its
    // debug inspector. The stamp changes when the file path changes.
    let path_stamp = fingerprint(path.as_bytes());
    let id = egui::Id::unique(("thinwire-photo-read", message_id, file_id, path_stamp));
    let active = egui::Id::unique(("thinwire-photo-active", message_id, file_id));
    retire_previous(ui, active, id);
    if let Some(slot) = ui.data(|data| data.get_temp::<BytesSlot>(id)) {
        return shown_from_slot(&slot, base);
    }
    let slot = BytesSlot::wait();
    let worker = slot.clone();
    let ctx = ui.ctx().clone();
    let path = path.to_owned();
    // A full queue waits for the next frame. The UI thread does not block.
    if !enqueue_photo(PhotoJob::Read {
        path,
        slot: worker,
        ctx,
    }) {
        return None;
    }
    track(ui.ctx(), slot.clone());
    ui.data_mut(|data| data.insert_temp(id, slot));
    None
}

fn shown_from_slot(slot: &BytesSlot, base: &str) -> Option<Shown> {
    match slot.view() {
        View::Ready { bytes, stamp } => Some(Shown {
            uri: image_uri(base, stamp),
            fresh: Some(bytes),
            slot: Some(slot.clone()),
        }),
        View::Cached { uri } => Some(Shown {
            uri,
            fresh: None,
            slot: None,
        }),
        View::Wait | View::Missing => None,
    }
}

fn blurred_mini(ui: &egui::Ui, message_id: &str, photo: &MessagePhoto) -> Option<Shown> {
    let mini = photo.mini.as_ref()?;
    let slice = mini.as_slice();
    if slice.is_empty() {
        return None;
    }
    let stamp = fingerprint(slice);
    let id = egui::Id::unique(("thinwire-photo-blur", message_id, stamp));
    let active = egui::Id::unique(("thinwire-photo-blur-active", message_id));
    retire_previous(ui, active, id);
    let base = mini_base(message_id);
    if let Some(slot) = ui.data(|data| data.get_temp::<BytesSlot>(id)) {
        return shown_from_slot(&slot, &base);
    }
    let slot = BytesSlot::wait();
    let worker = slot.clone();
    let ctx = ui.ctx().clone();
    let bytes = slice.to_vec();
    // A full queue waits for the next frame. The UI thread does not block.
    if !enqueue_photo(PhotoJob::Blur {
        bytes,
        slot: worker,
        ctx,
    }) {
        return None;
    }
    track(ui.ctx(), slot.clone());
    ui.data_mut(|data| data.insert_temp(id, slot));
    forget_base(ui, &base);
    None
}

fn retire_previous(ui: &egui::Ui, active: egui::Id, id: egui::Id) {
    let Some(previous) = ui.data(|data| data.get_temp::<egui::Id>(active)) else {
        ui.data_mut(|data| data.insert_temp(active, id));
        return;
    };
    if previous == id {
        return;
    }
    if let Some(slot) = ui.data(|data| data.get_temp::<BytesSlot>(previous)) {
        slot.abandon();
    }
    ui.data_mut(|data| {
        data.remove::<BytesSlot>(previous);
        data.insert_temp(active, id);
    });
}

enum PhotoJob {
    Read {
        path: String,
        slot: BytesSlot,
        ctx: egui::Context,
    },
    Blur {
        bytes: Vec<u8>,
        slot: BytesSlot,
        ctx: egui::Context,
    },
}

fn photo_sender() -> &'static mpsc::SyncSender<PhotoJob> {
    static SENDER: OnceLock<mpsc::SyncSender<PhotoJob>> = OnceLock::new();
    SENDER.get_or_init(|| {
        let (tx, rx) = mpsc::sync_channel(PHOTO_QUEUE);
        let rx = Arc::new(Mutex::new(rx));
        for index in 0..PHOTO_WORKERS {
            let rx = Arc::clone(&rx);
            let started = std::thread::Builder::new()
                .name(format!("thinwire-photo-{index}"))
                .spawn(move || {
                    loop {
                        let job = {
                            let guard = rx.lock().unwrap_or_else(|error| error.into_inner());
                            guard.recv()
                        };
                        match job {
                            Ok(job) => run_photo_job(job),
                            Err(_) => break,
                        }
                    }
                });
            if let Err(error) = started {
                tracing::warn!(%error, "photo worker did not start");
            }
        }
        tx
    })
}

/// `false` when the queue is full. The caller retries on a later frame.
#[must_use]
fn enqueue_photo(job: PhotoJob) -> bool {
    photo_sender().try_send(job).is_ok()
}

fn run_photo_job(job: PhotoJob) {
    match job {
        PhotoJob::Read { path, slot, ctx } => {
            let bytes = if slot.abandoned() {
                None
            } else {
                std::fs::read(&path).ok()
            };
            slot.finish(bytes);
            ctx.request_repaint();
        }
        PhotoJob::Blur { bytes, slot, ctx } => {
            let png = if slot.abandoned() {
                None
            } else {
                blur_png(&bytes)
            };
            slot.finish(png);
            ctx.request_repaint();
        }
    }
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

fn paint_shown(ui: &mut egui::Ui, base: &str, shown: &Shown, rect: egui::Rect) {
    if let Some(bytes) = &shown.fresh {
        publish(ui, base, &shown.uri, bytes);
        if let Some(slot) = &shown.slot {
            slot.hand_to_egui(shown.uri.clone());
        }
    }
    egui::Image::from_uri(&shown.uri)
        .fit_to_exact_size(rect.size())
        .show_loading_spinner(false)
        .paint_at(ui, rect);
}

/// Give egui these bytes. A new URI forgets the previous image for `base`,
/// so a replaced photo or a sharper size cannot keep the old texture.
fn publish(ui: &egui::Ui, base: &str, uri: &str, bytes: &[u8]) {
    let id = uri_id(base);
    let previous: Option<String> = ui.data(|data| data.get_temp(id));
    if previous.as_deref() == Some(uri) {
        return;
    }
    if let Some(old) = previous {
        ui.ctx().forget_image(&old);
    }
    ui.ctx().include_bytes(uri.to_owned(), bytes.to_vec());
    ui.data_mut(|data| data.insert_temp(id, uri.to_owned()));
}

fn forget_base(ui: &egui::Ui, base: &str) {
    let id = uri_id(base);
    let Some(uri) = ui.data(|data| data.get_temp::<String>(id)) else {
        return;
    };
    ui.ctx().forget_image(&uri);
    ui.data_mut(|data| data.remove::<String>(id));
}

fn remembered_uri(ui: &egui::Ui, base: &str) -> Option<String> {
    ui.data(|data| data.get_temp(uri_id(base)))
}

fn uri_id(base: &str) -> egui::Id {
    egui::Id::unique(("thinwire-photo-uri", base))
}

fn image_uri(base: &str, stamp: u64) -> String {
    format!("{base}/{stamp:x}")
}

fn photo_base(message_id: &str) -> String {
    format!("bytes://thinwire/photo/{message_id}")
}

fn mini_base(message_id: &str) -> String {
    format!("bytes://thinwire/mini/{message_id}")
}

fn fingerprint(bytes: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// The full photo, fitted in the window. Esc and Close leave it.
pub(crate) fn overlay(ui: &mut egui::Ui, messages: &[thinwire_protocol::ChatMessage]) {
    prune_loads(ui.ctx());
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
    let display = photo_box(photo.width, photo.height).x;
    let Some(shown) = selected_image(ui, &message.id, photo, display) else {
        return;
    };
    let screen = ui.ctx().content_rect();
    egui::Area::new(egui::Id::unique("photo-overlay"))
        .order(egui::Order::Foreground)
        .fixed_pos(screen.min)
        .show(ui.ctx(), |ui| {
            let _ = ui.allocate_rect(screen, Sense::click());
            ui.painter()
                .rect_filled(screen, 0.0, Color32::from_black_alpha(180));
            let margin = 48.0;
            let fitted = overlay_fit(photo.width, photo.height, screen.shrink(margin).size());
            let rect = egui::Rect::from_center_size(screen.center(), fitted);
            paint_shown(ui, &photo_base(&message.id), &shown, rect);
            let close = egui::Rect::from_min_size(
                screen.right_top() + egui::vec2(-88.0, 16.0),
                egui::vec2(72.0, 28.0),
            );
            if ui.put(close, egui::Button::new("Close")).clicked() {
                take_escape(ui.ctx());
            }
        });
}

/// Fit the photo's own pixel size into `bounds`. A large photo can grow past
/// the 360-point inline box. A photo smaller than the window stays at its
/// own size.
fn overlay_fit(width: u32, height: u32, bounds: Vec2) -> Vec2 {
    fit_in(Vec2::new(width.max(1) as f32, height.max(1) as f32), bounds)
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

/// Drop jobs whose bytes are already in egui, and jobs that failed.
fn prune_loads(ctx: &egui::Context) {
    let mut state = photo_ui(ctx);
    let before = state.loads.len();
    state.loads.retain(BytesSlot::holds_work);
    if state.loads.len() != before {
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
    egui::Id::unique("thinwire-photo-ui")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_visible_avatar_asks_for_its_file_once() {
        let mut avatar = ChatAvatar {
            file_id: Some(4),
            ..Default::default()
        };
        let ctx = egui::Context::default();
        super::super::theme::install(&ctx);
        let mut first = Vec::new();
        run_avatar(&ctx, ProtocolId::Telegram, "1", &avatar, &mut first);
        assert_eq!(avatar_asks(&first), 1);
        avatar.requested = true;
        let mut second = Vec::new();
        run_avatar(&ctx, ProtocolId::Telegram, "1", &avatar, &mut second);
        assert!(second.is_empty(), "the next frame asked again: {second:?}");
        avatar.requested = false;
        let mut other = Vec::new();
        run_avatar(&ctx, ProtocolId::Discord, "1", &avatar, &mut other);
        assert!(
            other.is_empty(),
            "another protocol asked for a photo: {other:?}"
        );
    }

    #[test]
    fn the_avatar_image_uri_includes_the_protocol() {
        let bytes = b"avatar-bytes";
        let avatar = ChatAvatar {
            file_id: Some(4),
            bytes: Some(thinwire_protocol::ImageBytes::new(bytes.to_vec())),
            ..Default::default()
        };
        let ctx = egui::Context::default();
        super::super::theme::install(&ctx);
        run_avatar(&ctx, ProtocolId::Telegram, "1", &avatar, &mut Vec::new());
        let uri = image_uri(&avatar_base(ProtocolId::Telegram, "1"), fingerprint(bytes));
        assert!(uri.contains("Telegram/1"), "{uri}");
        assert_eq!(cached_bytes(&ctx, &uri).as_deref(), Some(bytes.as_slice()));
        let discord = image_uri(&avatar_base(ProtocolId::Discord, "1"), fingerprint(bytes));
        assert_ne!(uri, discord);
        assert!(cached_bytes(&ctx, &discord).is_none());
    }

    #[test]
    fn initials_use_the_first_letter_of_up_to_two_words() {
        assert_eq!(initials("Ada Park"), "AP");
        assert_eq!(initials("Book club"), "BC");
        assert_eq!(initials("Milo"), "M");
        assert_eq!(initials("#design"), "D");
        assert_eq!(initials("   "), "?");
    }

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
    fn a_cached_smaller_size_still_requests_the_selected_one() {
        let mut waiting = sample_photo(false, false);
        waiting.width = 640;
        waiting.height = 320;
        waiting.sizes = vec![
            PhotoSize {
                width: 80,
                height: 40,
                file_id: Some(1),
                path: None,
                bytes: Some(thinwire_protocol::ImageBytes::new(vec![1, 2, 3])),
            },
            PhotoSize {
                width: 640,
                height: 320,
                file_id: Some(3),
                path: None,
                bytes: None,
            },
        ];
        let display = photo_box(waiting.width, waiting.height).x;
        assert_eq!(download_id(&waiting, display), Some(3));
        let ctx = egui::Context::default();
        super::super::theme::install(&ctx);
        let mut out = Vec::new();
        run_frame(&ctx, &waiting, &mut out);
        assert!(
            out.iter()
                .any(|intent| matches!(intent, Intent::LoadPhoto { file_id: 3, .. })),
            "the size for the bubble was not requested: {out:?}"
        );
        assert!(
            out.iter()
                .all(|intent| !matches!(intent, Intent::LoadPhoto { file_id: 1, .. })),
            "the cached thumbnail was requested: {out:?}"
        );

        waiting.sizes[1].bytes = Some(thinwire_protocol::ImageBytes::new(vec![4, 5, 6]));
        let mut ready = Vec::new();
        run_frame(&ctx, &waiting, &mut ready);
        assert!(
            ready.is_empty(),
            "a ready selected size requested another file: {ready:?}"
        );
    }

    #[test]
    fn the_overlay_fits_the_photo_to_the_window() {
        let inline = photo_box(2000, 1000);
        let bounds = Vec2::new(1000.0, 800.0);
        let fitted = overlay_fit(2000, 1000, bounds);
        assert!(
            fitted.x > inline.x + 100.0,
            "overlay {fitted:?} stayed at the inline box {inline:?}"
        );
        assert!(fitted.x <= bounds.x && fitted.y <= bounds.y);
        let small = overlay_fit(40, 40, bounds);
        assert!((small.x - 40.0).abs() < 0.1 && (small.y - 40.0).abs() < 0.1);
    }

    #[test]
    fn a_replaced_photo_forgets_the_previous_texture() {
        let ctx = egui::Context::default();
        super::super::theme::install(&ctx);
        let first = vec![1_u8, 2, 3];
        let second = vec![4_u8, 5, 6, 7];
        run_frame(&ctx, &inline_photo(&first), &mut Vec::new());
        let first_uri = image_uri(&photo_base("telegram:1:2"), fingerprint(&first));
        assert_eq!(
            cached_bytes(&ctx, &first_uri).as_deref(),
            Some(first.as_slice())
        );
        run_frame(&ctx, &inline_photo(&second), &mut Vec::new());
        let second_uri = image_uri(&photo_base("telegram:1:2"), fingerprint(&second));
        assert_ne!(first_uri, second_uri);
        assert_eq!(
            cached_bytes(&ctx, &second_uri).as_deref(),
            Some(second.as_slice())
        );
        assert!(
            cached_bytes(&ctx, &first_uri).is_none(),
            "the old texture is still cached"
        );
    }

    #[test]
    fn a_shown_file_drops_its_bytes_once_egui_has_them() {
        let bytes = b"thinwire-photo-bytes";
        let path = std::env::temp_dir().join(format!(
            "thinwire-photo-{}-{}.bin",
            std::process::id(),
            NEXT_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, bytes).expect("temp photo");
        let photo = MessagePhoto {
            width: 120,
            height: 80,
            mini: None,
            sizes: vec![PhotoSize {
                width: 120,
                height: 80,
                file_id: Some(4),
                path: Some(thinwire_protocol::ImagePath::new(
                    path.to_string_lossy().into_owned(),
                )),
                bytes: None,
            }],
            has_spoiler: false,
            is_secret: false,
            failed: false,
        };
        let ctx = egui::Context::default();
        super::super::theme::install(&ctx);
        let uri = image_uri(&photo_base("telegram:1:2"), fingerprint(bytes));
        let mut released = false;
        for _ in 0..80 {
            run_frame(&ctx, &photo, &mut Vec::new());
            if cached_bytes(&ctx, &uri).as_deref() == Some(bytes.as_slice())
                && !photo_ui(&ctx).loads.iter().any(BytesSlot::holds_bytes)
            {
                released = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let _ = std::fs::remove_file(&path);
        assert!(released, "the file bytes never reached egui");
        run_frame(&ctx, &photo, &mut Vec::new());
        assert!(
            photo_ui(&ctx).loads.is_empty(),
            "a finished read stayed in the job list"
        );
        assert_eq!(cached_bytes(&ctx, &uri).as_deref(), Some(bytes.as_slice()));
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

    fn inline_photo(bytes: &[u8]) -> MessagePhoto {
        let mut photo = sample_photo(false, false);
        photo.sizes[0].bytes = Some(thinwire_protocol::ImageBytes::new(bytes.to_vec()));
        photo
    }

    fn cached_bytes(ctx: &egui::Context, uri: &str) -> Option<Vec<u8>> {
        match ctx.try_load_bytes(uri) {
            Ok(egui::load::BytesPoll::Ready { bytes, .. }) => Some(bytes.to_vec()),
            _ => None,
        }
    }

    static NEXT_FILE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn avatar_asks(out: &[Intent]) -> usize {
        out.iter()
            .filter(|intent| matches!(intent, Intent::LoadAvatar { file_id: 4, .. }))
            .count()
    }

    fn run_avatar(
        ctx: &egui::Context,
        protocol: ProtocolId,
        conversation_id: &str,
        avatar: &ChatAvatar,
        out: &mut Vec<Intent>,
    ) {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(400.0, 200.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let rect = egui::Rect::from_min_size(
                    ui.cursor().min,
                    egui::vec2(AVATAR_SIDE, AVATAR_SIDE),
                );
                paint_avatar(ui, rect, protocol, conversation_id, "Ada", avatar, out);
            });
        });
        output.textures_delta.clear();
    }

    fn run_frame(ctx: &egui::Context, photo: &MessagePhoto, out: &mut Vec<Intent>) {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900.0, 700.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                show(
                    ui,
                    ProtocolId::Telegram,
                    "telegram:1",
                    "telegram:1:2",
                    photo,
                    out,
                );
            });
        });
        // The image loader queues a texture. These tests check bytes and layout.
        output.textures_delta.clear();
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

    #[test]
    fn photo_jobs_use_a_bounded_queue() {
        let src = include_str!("photos.rs");
        assert!(src.contains("sync_channel(PHOTO_QUEUE)"));
        assert!(src.contains("try_send"));
        assert!(!src.contains(concat!("std::thread::", "spawn")));
    }
}
