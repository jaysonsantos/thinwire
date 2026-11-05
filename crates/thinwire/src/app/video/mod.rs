//! In-app video (feature `video`, ADR 0015).
//!
//! One video plays at a time. Play on a clip with no file asks the core for
//! the download, and the clip starts when its path comes. The newest frame
//! takes the place of the thumbnail in the row.

mod player;

use eframe::egui::{self, RichText};
use thinwire_core::Intent;
use thinwire_protocol::{ChatMessage, MessageVideo, ProtocolId};

use super::{motion, theme};
use player::{PlayState, VideoPlayer};

/// The widest frame in a row, in points.
const MAX_WIDTH: f32 = 320.0;

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

    /// The frame of this clip is on screen: the row does not draw its
    /// thumbnail.
    pub(crate) fn covers(
        &self,
        protocol: ProtocolId,
        conversation_id: &str,
        message_id: &str,
    ) -> bool {
        matches!(
            &self.slot,
            Slot::Playing { key, texture: Some(_), .. } if is(key, protocol, conversation_id, message_id)
        )
    }

    /// The frame and the controls of the clip in one row.
    pub(crate) fn show(
        &mut self,
        ui: &mut egui::Ui,
        protocol: ProtocolId,
        conversation_id: &str,
        message_id: &str,
        video: &MessageVideo,
        out: &mut Vec<Intent>,
    ) {
        let key = VideoKey {
            protocol,
            conversation_id: conversation_id.to_owned(),
            message_id: message_id.to_owned(),
        };
        self.advance(ui.ctx(), &key, video);
        let palette = theme::palette(ui);
        let mut action = Action::None;
        ui.push_id(
            (conversation_id, message_id, "video"),
            |ui| match &mut self.slot {
                Slot::Playing {
                    key: playing,
                    player,
                    texture,
                } if *playing == key => {
                    if let Some(texture) = texture.as_ref() {
                        let size = texture.size_vec2();
                        let width = size.x.min(MAX_WIDTH).min(ui.available_width());
                        let scale = if size.x > 0.0 { width / size.x } else { 1.0 };
                        ui.add(egui::Image::new((texture.id(), size * scale)));
                    }
                    let state = player.poll();
                    ui.horizontal(|ui| {
                        if state == PlayState::Failed {
                            ui.label(
                                RichText::new("Couldn't play this video.")
                                    .small()
                                    .color(palette.error),
                            );
                            if small_button(ui, "Close") {
                                action = Action::Close;
                            }
                            return;
                        }
                        let label = match state {
                            PlayState::Paused => "Play",
                            PlayState::Ended => "Replay",
                            PlayState::Playing | PlayState::Failed => "Pause",
                        };
                        if small_button(ui, label) {
                            player.toggle();
                        }
                        if small_button(ui, "Close") {
                            action = Action::Close;
                        }
                    });
                }
                Slot::Waiting(waiting) if *waiting == key => {
                    ui.horizontal(|ui| {
                        motion::spinner(ui);
                        ui.label(RichText::new("Loading video…").small().color(palette.text2));
                    });
                }
                Slot::Failed(failed) if *failed == key => {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new("Couldn't play this video.")
                                .small()
                                .color(palette.error),
                        );
                        if small_button(ui, "Retry") {
                            action = Action::Press;
                        }
                    });
                }
                _ => {
                    let label = match video.duration_secs() {
                        0 => "Play video".to_owned(),
                        secs => format!("Play video {}:{:02}", secs / 60, secs % 60),
                    };
                    if small_button(ui, &label) {
                        action = Action::Press;
                    }
                }
            },
        );
        match action {
            Action::None => {}
            Action::Close => self.slot = Slot::Idle,
            Action::Press => self.press(ui.ctx(), key, video, out),
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
            self.open(ctx, key, path.as_path());
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
                    self.open(ctx, key.clone(), &path);
                } else if video.failed() {
                    self.slot = Slot::Failed(key.clone());
                }
            }
            Slot::Playing {
                key: playing,
                player,
                texture,
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

    fn open(&mut self, ctx: &egui::Context, key: VideoKey, path: &std::path::Path) {
        let repaint = ctx.clone();
        self.slot = match VideoPlayer::open(path, move || repaint.request_repaint()) {
            Ok(player) => Slot::Playing {
                key,
                player,
                texture: None,
            },
            Err(_) => {
                tracing::warn!("a video could not start");
                Slot::Failed(key)
            }
        };
    }
}

/// What a click in the row asks for.
enum Action {
    None,
    Close,
    Press,
}

fn small_button(ui: &mut egui::Ui, label: &str) -> bool {
    ui.add(egui::Button::new(RichText::new(label).small()).small())
        .clicked()
}

fn is(key: &VideoKey, protocol: ProtocolId, conversation_id: &str, message_id: &str) -> bool {
    key.protocol == protocol
        && key.conversation_id == conversation_id
        && key.message_id == message_id
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
        assert!(!slot.covers(ProtocolId::Telegram, "telegram:1", "telegram:1:5"));

        slot.sync(&[row(Some(clip))]);
        assert!(
            matches!(slot.slot, Slot::Failed(_)),
            "the clip is still here"
        );
        slot.sync(&[row(None)]);
        assert!(matches!(slot.slot, Slot::Idle), "the clip left the chat");
    }
}
