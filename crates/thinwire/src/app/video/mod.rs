//! In-app video (feature `video`, ADR 0015).
//!
//! One video plays at a time. A video file plays over the whole window. A
//! video message (`MessageVideo::is_note`) plays in its row, in the place of
//! its thumbnail. Play, or a tap on the thumbnail, on a clip with no file
//! asks the core for the download, and the clip starts when its path comes.
//! Esc, Close, and a click outside the frame close the view over the
//! window. A click on a video message pauses it, and Esc or Stop ends it.

mod player;

use eframe::egui::{self, RichText};
use thinwire_core::Intent;
use thinwire_protocol::{ChatMessage, MessageVideo, ProtocolId};

use super::{motion, theme};
use player::{PlayState, VideoPlayer};

/// How often a playing video checks its end with no new frame.
const POLL: std::time::Duration = std::time::Duration::from_millis(250);

#[derive(Clone, PartialEq, Eq, Debug)]
struct VideoKey {
    protocol: ProtocolId,
    conversation_id: String,
    message_id: String,
}

enum Slot {
    Idle,
    /// The download of this clip runs.
    Waiting(VideoKey),
    Playing {
        key: VideoKey,
        player: VideoPlayer,
        texture: Option<egui::TextureHandle>,
        /// The clip plays in its row, not over the window.
        inline: bool,
    },
    Failed(VideoKey),
}

/// The video of the open chat.
pub(crate) struct VideoSlot {
    slot: Slot,
}

impl Default for VideoSlot {
    fn default() -> Self {
        Self { slot: Slot::Idle }
    }
}

impl VideoSlot {
    /// The clip left the open chat: its player stops.
    pub(crate) fn sync(&mut self, messages: &[ChatMessage]) {
        let key = match &self.slot {
            Slot::Idle => return,
            Slot::Waiting(key) | Slot::Failed(key) | Slot::Playing { key, .. } => key,
        };
        let here = messages.iter().any(|message| {
            message.protocol == key.protocol
                && message.conversation_id == key.conversation_id
                && message.id == key.message_id
                && message.video.is_some()
        });
        if !here {
            self.slot = Slot::Idle;
        }
    }

    pub(crate) fn shutdown(&mut self) {
        self.slot = Slot::Idle;
    }

    /// Close the video view, as Esc does. `true` when one was open.
    pub(crate) fn take_escape(&mut self) -> bool {
        let open = !matches!(self.slot, Slot::Idle);
        self.slot = Slot::Idle;
        open
    }

    /// The controls of the clip in its row. `tapped` is a tap on the
    /// thumbnail: it plays the clip, as Play does.
    #[allow(
        clippy::too_many_arguments,
        reason = "one row: its ids, its clip, and the tap"
    )]
    pub(crate) fn show(
        &mut self,
        ui: &mut egui::Ui,
        protocol: ProtocolId,
        conversation_id: &str,
        message_id: &str,
        video: &MessageVideo,
        tapped: bool,
        out: &mut Vec<Intent>,
    ) {
        let key = VideoKey {
            protocol,
            conversation_id: conversation_id.to_owned(),
            message_id: message_id.to_owned(),
        };
        let palette = theme::palette(ui);
        let mut press = tapped;
        let mut stop = false;
        ui.push_id((conversation_id, message_id, "video"), |ui| {
            match &self.slot {
                Slot::Playing {
                    key: playing,
                    inline,
                    ..
                } if *playing == key => {
                    if *inline {
                        stop = small_button(ui, "Stop");
                    } else {
                        ui.label(RichText::new("Playing…").small().color(palette.text2));
                    }
                    press = false;
                }
                Slot::Waiting(waiting) if *waiting == key => {
                    ui.horizontal(|ui| {
                        motion::spinner(ui);
                        ui.label(RichText::new("Loading video…").small().color(palette.text2));
                    });
                    press = false;
                }
                Slot::Failed(failed) if *failed == key => {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new("Couldn't play this video.")
                                .small()
                                .color(palette.error),
                        );
                        if small_button(ui, "Retry") {
                            press = true;
                        }
                    });
                }
                _ => {
                    let label = match video.duration_secs() {
                        0 => "Play video".to_owned(),
                        secs => format!("Play video {}:{:02}", secs / 60, secs % 60),
                    };
                    if small_button(ui, &label) {
                        press = true;
                    }
                }
            }
        });
        if stop {
            self.slot = Slot::Idle;
        } else if press {
            self.press(ui.ctx(), key, video, out);
        }
    }

    /// The frame of a video message that plays in its row. It has the place
    /// and the size of the thumbnail, so the row does not move. A click
    /// pauses the clip, plays it again, or starts it again after its end.
    /// `false`: this row does not play, and the caller draws the thumbnail.
    pub(crate) fn show_inline(
        &mut self,
        ui: &mut egui::Ui,
        protocol: ProtocolId,
        conversation_id: &str,
        message_id: &str,
        video: &MessageVideo,
    ) -> bool {
        let Slot::Playing {
            key,
            player,
            texture: Some(texture),
            inline: true,
        } = &mut self.slot
        else {
            return false;
        };
        if key.protocol != protocol
            || key.conversation_id != conversation_id
            || key.message_id != message_id
        {
            return false;
        }
        let size = match video.size() {
            (0, _) | (_, 0) => {
                let [width, height] = texture.size();
                super::photos::photo_box(width as u32, height as u32)
            }
            (width, height) => super::photos::photo_box(width, height),
        };
        let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
        egui::Image::from_texture(&*texture)
            .corner_radius(theme::radius::ROW)
            .paint_at(ui, rect);
        let state = player.poll();
        let name = match state {
            PlayState::Playing => "Pause video message",
            PlayState::Paused | PlayState::Ended | PlayState::Failed => "Play video message",
        };
        response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, name));
        if response.clicked() {
            player.toggle();
        }
        true
    }

    /// The playing clip over the whole window. A waiting clip starts here
    /// when its file comes. Close and a click outside the frame close it.
    pub(crate) fn overlay(&mut self, ui: &mut egui::Ui, messages: &[ChatMessage]) {
        if let Slot::Waiting(key) = &self.slot {
            let key = key.clone();
            if let Some(video) = video_of(messages, &key) {
                self.advance(ui.ctx(), &key, &video);
            }
        }
        if let Slot::Playing { key, .. } = &self.slot {
            let key = key.clone();
            if let Some(video) = video_of(messages, &key) {
                self.advance(ui.ctx(), &key, &video);
            }
        }
        let Slot::Playing {
            player,
            texture,
            inline: false,
            ..
        } = &mut self.slot
        else {
            return;
        };
        let state = player.poll();
        let screen = ui.ctx().content_rect();
        let mut action = Action::None;
        egui::Area::new(egui::Id::unique("video-overlay"))
            .order(egui::Order::Foreground)
            .fixed_pos(screen.min)
            .show(ui.ctx(), |ui| {
                let backdrop = ui.allocate_rect(screen, egui::Sense::click());
                ui.painter()
                    .rect_filled(screen, 0.0, egui::Color32::from_black_alpha(200));
                let room = screen.shrink(48.0);
                let bar = 40.0;
                let space = egui::vec2(room.width(), (room.height() - bar).max(1.0));
                let frame = texture.as_ref().map_or(egui::Vec2::splat(64.0), |texture| {
                    let size = texture.size_vec2();
                    let scale = (space.x / size.x).min(space.y / size.y).clamp(0.0, 1.0);
                    size * scale
                });
                let rect = egui::Rect::from_center_size(
                    screen.center() - egui::vec2(0.0, bar / 2.0),
                    frame,
                );
                match texture.as_ref() {
                    Some(texture) => {
                        ui.painter().image(
                            texture.id(),
                            rect,
                            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                            egui::Color32::WHITE,
                        );
                    }
                    None => {
                        let _ = ui.place(rect, |ui: &mut egui::Ui| {
                            ui.centered_and_justified(motion::spinner).response
                        });
                    }
                }
                let controls = egui::Rect::from_center_size(
                    egui::pos2(screen.center().x, rect.bottom() + bar / 2.0 + 4.0),
                    egui::vec2(220.0, 28.0),
                );
                ui.scope_builder(egui::UiBuilder::new().max_rect(controls), |ui| {
                    ui.horizontal_centered(|ui| {
                        if state == PlayState::Failed {
                            ui.label(
                                RichText::new("Couldn't play this video.")
                                    .small()
                                    .color(egui::Color32::WHITE),
                            );
                        } else {
                            let label = match state {
                                PlayState::Paused => "Play",
                                PlayState::Ended => "Replay",
                                PlayState::Playing | PlayState::Failed => "Pause",
                            };
                            if ui.button(label).clicked() {
                                action = Action::Toggle;
                            }
                        }
                        if ui.button("Close").clicked() {
                            action = Action::Close;
                        }
                    });
                });
                let outside = backdrop.clicked()
                    && super::photos::outside(backdrop.interact_pointer_pos(), &[rect, controls]);
                if outside {
                    action = Action::Close;
                }
            });
        match action {
            Action::None => {}
            Action::Toggle => player.toggle(),
            Action::Close => self.slot = Slot::Idle,
        }
    }

    /// Play the clip now when its file is here. Else ask for the download.
    fn press(
        &mut self,
        ctx: &egui::Context,
        key: VideoKey,
        video: &MessageVideo,
        out: &mut Vec<Intent>,
    ) {
        if let Some(path) = video.file() {
            self.open(ctx, key, path.as_path(), video.is_note());
            return;
        }
        out.push(Intent::PlayVideo {
            protocol: key.protocol,
            conversation_id: key.conversation_id.clone(),
            message_id: key.message_id.clone(),
            file_id: video.file_id(),
        });
        self.slot = Slot::Waiting(key);
    }

    /// A waiting clip starts when its file comes, or fails with the
    /// download. A playing clip gets its newest frame.
    fn advance(&mut self, ctx: &egui::Context, key: &VideoKey, video: &MessageVideo) {
        match &mut self.slot {
            Slot::Waiting(waiting) if waiting == key => {
                if let Some(path) = video.file() {
                    let path = path.as_path().to_path_buf();
                    self.open(ctx, key.clone(), &path, video.is_note());
                } else if video.failed() {
                    self.slot = Slot::Failed(key.clone());
                }
            }
            Slot::Playing {
                key: playing,
                player,
                texture,
                ..
            } if playing == key => {
                if let Some(frame) = player.take_frame() {
                    let image = egui::ColorImage::from_rgba_unmultiplied(
                        [frame.width as usize, frame.height as usize],
                        &frame.rgba,
                    );
                    match texture {
                        Some(texture) => texture.set(image, egui::TextureOptions::LINEAR),
                        None => {
                            *texture = Some(ctx.load_texture(
                                "thinwire-video",
                                image,
                                egui::TextureOptions::LINEAR,
                            ));
                        }
                    }
                }
                if player.poll() == PlayState::Playing {
                    ctx.request_repaint_after(POLL);
                }
            }
            _ => {}
        }
    }

    fn open(&mut self, ctx: &egui::Context, key: VideoKey, path: &std::path::Path, inline: bool) {
        let repaint = ctx.clone();
        self.slot = match VideoPlayer::open(path, move || repaint.request_repaint()) {
            Ok(player) => Slot::Playing {
                key,
                player,
                texture: None,
                inline,
            },
            Err(_) => {
                tracing::warn!("a video could not start");
                Slot::Failed(key)
            }
        };
    }
}

/// What a click in the video view asks for.
enum Action {
    None,
    Toggle,
    Close,
}

fn small_button(ui: &mut egui::Ui, label: &str) -> bool {
    ui.add(egui::Button::new(RichText::new(label).small()).small())
        .clicked()
}

/// The clip of the message `key`, while it is in the open chat.
fn video_of(messages: &[ChatMessage], key: &VideoKey) -> Option<MessageVideo> {
    messages
        .iter()
        .find(|message| {
            message.protocol == key.protocol
                && message.conversation_id == key.conversation_id
                && message.id == key.message_id
        })
        .and_then(|message| message.video.clone())
}

#[cfg(test)]
mod tests {
    use thinwire_protocol::{Arrival, Delivery};

    use super::*;

    fn key() -> VideoKey {
        VideoKey {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            message_id: "telegram:1:5".into(),
        }
    }

    fn row(video: Option<MessageVideo>) -> ChatMessage {
        ChatMessage {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            id: "telegram:1:5".into(),
            sender: "Ada".into(),
            body: "Video".into(),
            outbound: false,
            delivery: Delivery::Sent,
            sent_at: 0,
            arrival: Arrival::History,
            topic: None,
            links: Vec::new(),
            photo: None,
            audio: None,
            video,
            file: None,
            reactions: Vec::new(),
            reply_to: None,
            edited: false,
        }
    }

    /// Play with no file waits and asks the core for the download once. A
    /// failed download shows Retry. A clip that leaves the chat stops.
    #[test]
    fn play_waits_for_the_download_and_a_failure_offers_retry() {
        let ctx = egui::Context::default();
        let clip = MessageVideo::new(12, 640, 360, 31, None).expect("clip");
        let mut slot = VideoSlot::default();
        let mut out = Vec::new();
        slot.press(&ctx, key(), &clip, &mut out);
        assert_eq!(
            out,
            vec![Intent::PlayVideo {
                protocol: ProtocolId::Telegram,
                conversation_id: "telegram:1".into(),
                message_id: "telegram:1:5".into(),
                file_id: 31,
            }]
        );
        assert!(matches!(slot.slot, Slot::Waiting(_)));
        slot.advance(&ctx, &key(), &clip);
        assert!(matches!(slot.slot, Slot::Waiting(_)), "still downloading");
        let mut failed = clip.clone();
        failed.mark_failed();
        slot.advance(&ctx, &key(), &failed);
        assert!(matches!(slot.slot, Slot::Failed(_)));

        slot.sync(&[row(Some(clip))]);
        assert!(
            matches!(slot.slot, Slot::Failed(_)),
            "the clip is still here"
        );
        slot.sync(&[row(None)]);
        assert!(matches!(slot.slot, Slot::Idle), "the clip left the chat");
    }

    /// A video message that plays has its frame in its row. Another row,
    /// and a video file, keep the thumbnail: a video file plays over the
    /// window.
    #[test]
    fn a_video_message_plays_in_its_row() {
        let dir = std::env::temp_dir()
            .join("thinwire-video-tests")
            .join(format!("{}-inline", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("clip.mp4");
        std::fs::write(&path, include_bytes!("fixtures/clip.mp4")).expect("write");
        let note = MessageVideo::new(1, 64, 36, 31, None)
            .expect("clip")
            .into_note();
        let ctx = egui::Context::default();
        let mut slot = VideoSlot::default();
        let drawn = |slot: &mut VideoSlot, message_id: &str| {
            let mut drawn = false;
            let mut output = ctx.run_ui(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    drawn =
                        slot.show_inline(ui, ProtocolId::Telegram, "telegram:1", message_id, &note);
                });
            });
            output.textures_delta.clear();
            drawn
        };
        assert!(!drawn(&mut slot, "telegram:1:5"), "nothing plays");

        for inline in [true, false] {
            slot.slot = Slot::Playing {
                key: key(),
                player: VideoPlayer::open_with(&path, Some("fakesink"), || {})
                    .expect("the clip starts"),
                texture: None,
                inline,
            };
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while std::time::Instant::now() < deadline
                && !matches!(
                    &slot.slot,
                    Slot::Playing {
                        texture: Some(_),
                        ..
                    }
                )
            {
                slot.advance(&ctx, &key(), &note);
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            assert!(
                matches!(
                    &slot.slot,
                    Slot::Playing {
                        texture: Some(_),
                        ..
                    }
                ),
                "a frame came"
            );
            assert_eq!(drawn(&mut slot, "telegram:1:5"), inline);
            assert!(!drawn(&mut slot, "telegram:1:6"), "another row");
        }
        slot.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
