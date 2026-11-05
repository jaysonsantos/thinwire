//! Photos and videos for the compose field: drop, paste, and the tray.
//!
//! A dropped file and a file pasted from a file manager go to the core as a
//! path. A pasted image is saved as a PNG file in the cache folder first, on
//! its own thread, so the UI thread does no encoding. The core keeps the tray
//! of each thread and skips a file that is not a photo or a video.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use eframe::egui::{self, RichText};
use thinwire_core::{Intent, View};
use thinwire_protocol::{MediaKind, MessageTopic, ProtocolId};

use super::theme::{self, radius, space};

/// The thread the tray and the pasted files belong to.
pub(super) type Thread = (ProtocolId, String, Option<MessageTopic>);

/// Height of the row of file chips.
const CHIP_HEIGHT: f32 = 28.0;

/// Pasted images that a thread saved, waiting for the next frame.
#[derive(Clone, Default)]
struct Saved(Arc<Mutex<Vec<(Thread, PathBuf)>>>);

fn saved(ctx: &egui::Context) -> Saved {
    ctx.data_mut(|data| {
        data.get_temp_mut_or_default::<Saved>(egui::Id::unique("thinwire-pasted-images"))
            .clone()
    })
}

/// Height that the tray and its note take above the field. Zero when the
/// thread has no files and no note.
pub(super) fn tray_height(ui: &egui::Ui, snapshot: &View<'_>) -> f32 {
    let mut height = 0.0;
    if !snapshot.compose_attachments().is_empty() {
        height += CHIP_HEIGHT + space::S;
    }
    if snapshot.attachment_note().is_some() {
        height += ui.text_style_height(&egui::TextStyle::Small) + space::XS;
    }
    height
}

/// Turn the dropped files, the pasted files, and the saved pasted images of
/// this frame into intents for `thread`. Only a thread that can take files
/// calls this. A paste of plain text stays in the input for the field.
pub(super) fn take_input(
    ui: &mut egui::Ui,
    thread: &Thread,
    compose_focused: bool,
    out: &mut Vec<Intent>,
) {
    let ctx = ui.ctx().clone();
    let mut paths: Vec<PathBuf> = ui.input(|input| {
        input
            .raw
            .dropped_files
            .iter()
            .map(|file| file.path().to_path_buf())
            .filter(|path| !path.as_os_str().is_empty())
            .collect()
    });
    if compose_focused {
        paste(ui, thread, &mut paths);
    }
    let ready: Vec<PathBuf> = {
        let saved = saved(&ctx);
        let mut queue = saved
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut mine = Vec::new();
        queue.retain(|(owner, path)| {
            if owner == thread {
                mine.push(path.clone());
                false
            } else {
                true
            }
        });
        mine
    };
    paths.extend(ready);
    if !paths.is_empty() {
        let (protocol, conversation_id, topic) = thread.clone();
        out.push(Intent::AddAttachments {
            protocol,
            conversation_id,
            topic,
            paths,
        });
    }
}

/// A paste of files from a file manager adds the files, and the text of
/// the paste does not go into the field. A pasted image is saved first.
fn paste(ui: &mut egui::Ui, thread: &Thread, paths: &mut Vec<PathBuf>) {
    let (text_paste, images) = ui.input(|input| {
        let text = input
            .events
            .iter()
            .any(|event| matches!(event, egui::Event::Paste(_)));
        let images: Vec<Arc<egui::ColorImage>> = input
            .events
            .iter()
            .filter_map(|event| match event {
                egui::Event::PasteImage(image) => Some(image.clone()),
                _ => None,
            })
            .collect();
        (text, images)
    });
    if text_paste {
        let files = clipboard_files();
        if !files.is_empty() {
            paths.extend(files);
            ui.input_mut(|input| {
                input
                    .events
                    .retain(|event| !matches!(event, egui::Event::Paste(_)));
            });
        }
    }
    for image in images {
        save_pasted(ui.ctx(), thread.clone(), image);
    }
}

/// The files on the clipboard, when a file manager copied files. Empty for
/// text, or when the clipboard cannot be read.
fn clipboard_files() -> Vec<PathBuf> {
    #[cfg(not(test))]
    {
        arboard::Clipboard::new()
            .and_then(|mut clipboard| clipboard.get().file_list())
            .unwrap_or_default()
    }
    #[cfg(test)]
    {
        Vec::new()
    }
}

/// Save a pasted image as a PNG file on its own thread. The next frame adds
/// it to the tray of `thread`.
fn save_pasted(ctx: &egui::Context, thread: Thread, image: Arc<egui::ColorImage>) {
    let queue = saved(ctx);
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        match write_png(&image) {
            Ok(path) => {
                queue
                    .0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push((thread, path));
                ctx.request_repaint();
            }
            // The error can hold a path. It stays out of the log.
            Err(_) => tracing::warn!("a pasted image could not be saved"),
        }
    });
}

/// The folder for pasted images: only the user can read it. Tests use a
/// folder of their own.
fn paste_folder() -> std::io::Result<PathBuf> {
    #[cfg(not(test))]
    let folder = dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("thinwire")
        .join("pasted");
    #[cfg(test)]
    let folder = std::env::temp_dir().join(format!("thinwire-pasted-test-{}", std::process::id()));
    std::fs::create_dir_all(&folder)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o700))?;
    }
    remove_old(&folder);
    Ok(folder)
}

/// Pasted images older than a day were sent or dropped. Remove them.
fn remove_old(folder: &std::path::Path) {
    const DAY: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);
    let Ok(entries) = std::fs::read_dir(folder) else {
        return;
    };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > DAY);
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn write_png(image: &egui::ColorImage) -> std::io::Result<PathBuf> {
    let [width, height] = image.size;
    let bytes: Vec<u8> = image
        .pixels
        .iter()
        .flat_map(|pixel| pixel.to_srgba_unmultiplied())
        .collect();
    let width = u32::try_from(width).map_err(std::io::Error::other)?;
    let height = u32::try_from(height).map_err(std::io::Error::other)?;
    let buffer = image::RgbaImage::from_raw(width, height, bytes)
        .ok_or_else(|| std::io::Error::other("image size"))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let path = paste_folder()?.join(format!("pasted-{stamp}.png"));
    buffer
        .save_with_format(&path, image::ImageFormat::Png)
        .map_err(std::io::Error::other)?;
    Ok(path)
}

/// The files of the open thread, each with a remove button, and the note of
/// the last attach.
pub(super) fn tray(ui: &mut egui::Ui, snapshot: &View<'_>, thread: &Thread, out: &mut Vec<Intent>) {
    let files = snapshot.compose_attachments();
    let palette = theme::palette(ui);
    if !files.is_empty() {
        egui::ScrollArea::horizontal()
            .id_salt("compose-tray")
            .max_height(CHIP_HEIGHT)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = space::XS;
                    for (index, file) in files.iter().enumerate() {
                        let kind = match file.kind {
                            MediaKind::Photo => "Photo",
                            MediaKind::Video => "Video",
                        };
                        let name = file.file_name();
                        egui::Frame::new()
                            .fill(palette.surface)
                            .corner_radius(egui::CornerRadius::same(radius::PILL))
                            .inner_margin(egui::Margin::symmetric(space::S as i8, 2))
                            .show(ui, |ui| {
                                ui.set_height(CHIP_HEIGHT - 4.0);
                                ui.label(RichText::new(kind).small().color(palette.text3));
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(&name).small().color(palette.text),
                                    )
                                    .truncate(),
                                );
                                let remove = ui
                                    .add(
                                        egui::Button::new(RichText::new("×").color(palette.text2))
                                            .frame(false),
                                    )
                                    .on_hover_text("Remove");
                                remove.widget_info(|| {
                                    egui::WidgetInfo::labeled(
                                        egui::Role::Button,
                                        true,
                                        format!("Remove {name}"),
                                    )
                                });
                                if remove.clicked() {
                                    let (protocol, conversation_id, topic) = thread.clone();
                                    out.push(Intent::RemoveAttachment {
                                        protocol,
                                        conversation_id,
                                        topic,
                                        index,
                                    });
                                }
                            });
                    }
                });
            });
        ui.add_space(space::S);
    }
    if let Some(note) = snapshot.attachment_note() {
        ui.label(RichText::new(note).small().color(palette.warn));
        ui.add_space(space::XS);
    }
}

/// A frame over the thread while the pointer carries files.
pub(super) fn drop_hint(ui: &egui::Ui, rect: egui::Rect) {
    let hovering = ui.input(|input| !input.raw.hovered_files.is_empty());
    if !hovering {
        return;
    }
    let palette = theme::palette(ui);
    let painter = ui.ctx().layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::unique("thinwire-drop-hint"),
    ));
    painter.rect_filled(
        rect.shrink(space::S),
        egui::CornerRadius::same(radius::CARD),
        palette.bg.gamma_multiply(0.92),
    );
    painter.rect_stroke(
        rect.shrink(space::S),
        egui::CornerRadius::same(radius::CARD),
        egui::Stroke::new(2.0, palette.accent),
        egui::StrokeKind::Inside,
    );
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        "Drop photos and videos to attach",
        egui::FontId::new(16.0, theme::semibold()),
        palette.text,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pasted image becomes a PNG file that only the user can read.
    #[test]
    fn a_pasted_image_is_saved_as_png() {
        let image = egui::ColorImage::new([3, 2], vec![egui::Color32::RED; 6]);
        let path = write_png(&image).expect("saved");
        assert_eq!(path.extension().and_then(|ext| ext.to_str()), Some("png"));
        let decoded = image::open(&path).expect("decode");
        assert_eq!((decoded.width(), decoded.height()), (3, 2));
        let _ = std::fs::remove_file(&path);
    }
}
