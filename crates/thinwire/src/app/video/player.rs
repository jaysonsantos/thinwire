//! One video file, played with GStreamer (ADR 0015).
//!
//! `playbin` reads the file, plays its sound through the audio sink of the
//! platform, and keeps the picture in time with it. An `appsink` gets each
//! frame as RGBA. The callback keeps the newest frame and asks the UI to
//! paint again. The UI thread never waits on GStreamer.

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use gstreamer_video::prelude::*;

/// One decoded picture, with rows packed: four bytes for each pixel.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Frame({}x{})", self.width, self.height)
    }
}

/// What the player does now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlayState {
    Playing,
    Paused,
    /// The end of the file. Play starts it again from the start.
    Ended,
    /// GStreamer could not play the file. The log has the class, no path.
    Failed,
}

/// The player could not start. It holds no path and no GStreamer text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VideoError {
    /// GStreamer or one of its elements is not installed.
    Unavailable,
    /// The file did not start to play.
    NotPlayable,
}

/// The largest picture the player keeps, in pixels for each side. A larger
/// frame is dropped, so a bad file does not fill the memory.
const MAX_SIDE: u32 = 4096;

pub(crate) struct VideoPlayer {
    playbin: gst::Element,
    latest: Arc<Mutex<Option<Frame>>>,
    state: PlayState,
}

impl std::fmt::Debug for VideoPlayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoPlayer")
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl VideoPlayer {
    /// Start to play the file at `path`. `on_frame` runs on a GStreamer
    /// thread after each new frame: it asks the UI to paint again.
    pub(crate) fn open(
        path: &Path,
        on_frame: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self, VideoError> {
        Self::open_with(path, None, on_frame)
    }

    /// As [`Self::open`]. `audio_sink` replaces the audio sink of the
    /// platform: a test plays no sound.
    pub(crate) fn open_with(
        path: &Path,
        audio_sink: Option<&str>,
        on_frame: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self, VideoError> {
        init()?;
        let uri = gst::glib::filename_to_uri(path, None).map_err(|_| VideoError::NotPlayable)?;
        let playbin = gst::ElementFactory::make("playbin")
            .property("uri", uri.as_str())
            .build()
            .map_err(|_| VideoError::Unavailable)?;
        let caps = gst_video::VideoCapsBuilder::new()
            .format(gst_video::VideoFormat::Rgba)
            .build();
        let sink = gst_app::AppSink::builder()
            .caps(&caps)
            .max_buffers(2)
            .drop(true)
            .build();
        let latest = Arc::new(Mutex::new(None));
        let slot = Arc::clone(&latest);
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    if let Some(frame) = frame_of(&sample) {
                        *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(frame);
                        on_frame();
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        playbin.set_property("video-sink", &sink);
        if let Some(name) = audio_sink {
            let audio = gst::ElementFactory::make(name)
                .build()
                .map_err(|_| VideoError::Unavailable)?;
            playbin.set_property("audio-sink", &audio);
        }
        playbin
            .set_state(gst::State::Playing)
            .map_err(|_| VideoError::NotPlayable)?;
        Ok(Self {
            playbin,
            latest,
            state: PlayState::Playing,
        })
    }

    /// The newest frame since the last call, if one came.
    pub(crate) fn take_frame(&self) -> Option<Frame> {
        self.latest
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    /// Read the end and the errors from the bus, and return the state.
    pub(crate) fn poll(&mut self) -> PlayState {
        let Some(bus) = self.playbin.bus() else {
            return self.state;
        };
        while let Some(message) = bus.pop() {
            match message.view() {
                gst::MessageView::Eos(_) => self.state = PlayState::Ended,
                gst::MessageView::Error(_) => {
                    // The GStreamer text can name the path. It stays out.
                    tracing::warn!("a video could not be played");
                    self.state = PlayState::Failed;
                }
                _ => {}
            }
        }
        self.state
    }

    /// Pause a video that plays. Play a paused one. Play an ended one from
    /// the start.
    pub(crate) fn toggle(&mut self) {
        let next = match self.state {
            PlayState::Playing => PlayState::Paused,
            PlayState::Paused => PlayState::Playing,
            PlayState::Ended => {
                let _ = self.playbin.seek_simple(
                    gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                    gst::ClockTime::ZERO,
                );
                PlayState::Playing
            }
            PlayState::Failed => return,
        };
        let target = if next == PlayState::Playing {
            gst::State::Playing
        } else {
            gst::State::Paused
        };
        if self.playbin.set_state(target).is_ok() {
            self.state = next;
        } else {
            self.state = PlayState::Failed;
        }
    }
}

impl Drop for VideoPlayer {
    fn drop(&mut self) {
        let _ = self.playbin.set_state(gst::State::Null);
    }
}

/// The plugin folders of the Nix shell that built this binary. A release
/// build has none: it uses the plugins of the system or of its bundle.
const BUILD_PLUGIN_PATH: Option<&str> = option_env!("GST_PLUGIN_SYSTEM_PATH_1_0");

/// Start GStreamer one time. A local build that runs outside its Nix
/// shell does not see the shell plugins, and `playbin` is in one of them.
/// So when the environment names no plugin folder, the plugin folders of
/// the build shell are scanned.
fn init() -> Result<(), VideoError> {
    static STARTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let started = *STARTED.get_or_init(|| {
        if gst::init().is_err() {
            return false;
        }
        let named = [
            "GST_PLUGIN_SYSTEM_PATH_1_0",
            "GST_PLUGIN_SYSTEM_PATH",
            "GST_PLUGIN_PATH",
        ]
        .iter()
        .any(|name| std::env::var_os(name).is_some());
        if !named && let Some(paths) = BUILD_PLUGIN_PATH {
            let registry = gst::Registry::get();
            for dir in std::env::split_paths(paths).filter(|dir| dir.is_dir()) {
                registry.scan_path(&dir);
            }
        }
        true
    });
    if started {
        Ok(())
    } else {
        Err(VideoError::Unavailable)
    }
}

/// The RGBA picture of one sample, with its rows packed.
fn frame_of(sample: &gst::Sample) -> Option<Frame> {
    let buffer = sample.buffer()?;
    let info = gst_video::VideoInfo::from_caps(sample.caps()?).ok()?;
    let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info).ok()?;
    let (width, height) = (frame.width(), frame.height());
    if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
        return None;
    }
    let stride = usize::try_from(*frame.plane_stride().first()?).ok()?;
    let row = usize::try_from(width).ok()? * 4;
    let data = frame.plane_data(0).ok()?;
    let mut rgba = Vec::with_capacity(row * usize::try_from(height).ok()?);
    for line in data.chunks(stride).take(usize::try_from(height).ok()?) {
        rgba.extend_from_slice(line.get(..row)?);
    }
    Some(Frame {
        width,
        height,
        rgba,
    })
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    /// The clip is 0.6 s of a 64x36 test picture with a tone: H.264 and
    /// AAC in MP4, made with ffmpeg. It plays to its end with no sound.
    #[test]
    fn an_mp4_clip_plays_to_its_end() {
        let dir = std::env::temp_dir()
            .join("thinwire-video-tests")
            .join(std::process::id().to_string());
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("clip.mp4");
        std::fs::write(&path, include_bytes!("fixtures/clip.mp4")).expect("write");
        let frames = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = Arc::clone(&frames);
        let mut player = VideoPlayer::open_with(&path, Some("fakesink"), move || {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        })
        .expect("the clip starts");
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = None;
        while Instant::now() < deadline {
            if let Some(frame) = player.take_frame() {
                seen = Some(frame);
            }
            if player.poll() == PlayState::Ended {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(player.poll(), PlayState::Ended, "the clip ends");
        let frame = seen.expect("a frame");
        assert_eq!((frame.width, frame.height), (64, 36));
        assert_eq!(frame.rgba.len(), 64 * 36 * 4);
        assert!(frames.load(std::sync::atomic::Ordering::SeqCst) > 1);
        player.toggle();
        assert_eq!(player.poll(), PlayState::Playing, "play starts it again");
        drop(player);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_that_is_not_a_video_fails() {
        let dir = std::env::temp_dir()
            .join("thinwire-video-tests")
            .join(format!("{}-bad", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("not-a-video.mp4");
        std::fs::write(&path, b"not a video").expect("write");
        let mut player = match VideoPlayer::open_with(&path, Some("fakesink"), || {}) {
            Ok(player) => player,
            Err(error) => {
                assert_eq!(error, VideoError::NotPlayable);
                return;
            }
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && player.poll() != PlayState::Failed {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(player.poll(), PlayState::Failed);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
