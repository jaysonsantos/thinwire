//! Startup: take the single-instance lock before any protocol starts.
//!
//! A new launch right after a close often finds the old process still
//! closing its clients (the window hides at once, the process stays up to
//! the close limit). So a held lock means "wait": try again until
//! [`LOCK_WAIT`]. A short wait shows no window. After [`NOTE_AFTER`] a small
//! window says that thinwire is still closing. If the lock is still held at
//! the end, the window explains what happened instead of starting a second
//! copy.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use eframe::egui;
use thinwire_core::instance::{InstanceLock, LockError, RETRY_STEP};

use super::{SHUTDOWN_TIMEOUT, Settings, ThinwireApp, WATCHDOG_MARGIN};

/// How long a new launch waits for the old one to quit. An old process
/// ends at most [`SHUTDOWN_TIMEOUT`] plus the watchdog margin after its
/// close, so this covers a launch at any time after that close.
pub const LOCK_WAIT: Duration = SHUTDOWN_TIMEOUT.saturating_add(WATCHDOG_MARGIN);
/// A wait up to this long shows no window.
pub const NOTE_AFTER: Duration = Duration::from_secs(1);

/// Size of the small startup window.
pub const SMALL_SIZE: [f32; 2] = [440.0, 220.0];
/// Size of the failure window: room for the text and both buttons.
pub const FAILED_SIZE: [f32; 2] = [480.0, 300.0];
/// Size of the main window.
pub const MAIN_SIZE: [f32; 2] = [1120.0, 720.0];

pub(crate) const CLOSING_NOTE: &str = "thinwire is still closing…";
pub(crate) const FAILED_TITLE: &str = "thinwire did not start";
pub(crate) const TRY_AGAIN: &str = "Try again";
pub(crate) const QUIT: &str = "Quit";

/// The lock of this process. A static is never dropped, so the lock stays
/// until the process ends and the OS releases it.
static HELD: OnceLock<InstanceLock> = OnceLock::new();

fn hold(lock: InstanceLock) {
    tracing::debug!(path = %lock.path().display(), "single-instance lock taken");
    let _ = HELD.set(lock);
}

/// Where startup is after the first short wait.
#[derive(Debug)]
pub enum Startup {
    /// The lock is ours, or this platform cannot lock: start now.
    Ready,
    /// Another process holds the lock: wait with a small window.
    Waiting { path: PathBuf, deadline: Instant },
}

impl Startup {
    /// Window size to open with.
    #[must_use]
    pub const fn window_size(&self) -> [f32; 2] {
        match self {
            Self::Ready => MAIN_SIZE,
            Self::Waiting { .. } => SMALL_SIZE,
        }
    }
}

/// Take the lock at `path`, waiting up to [`NOTE_AFTER`] with no window.
/// Blocks the caller: call it before the window opens.
#[must_use]
pub fn first_try(path: Option<PathBuf>, launch: Instant) -> Startup {
    let Some(path) = path else {
        tracing::warn!("no data folder: starting without the single-instance lock");
        return Startup::Ready;
    };
    match InstanceLock::acquire_until(&path, launch + NOTE_AFTER) {
        Ok(lock) => {
            hold(lock);
            Startup::Ready
        }
        Err(LockError::Held) => {
            tracing::info!("another thinwire holds the lock; waiting for it to quit");
            Startup::Waiting {
                path,
                deadline: launch + LOCK_WAIT,
            }
        }
        Err(LockError::Io(error)) => {
            start_without_lock(&error);
            Startup::Ready
        }
    }
}

/// A lock that cannot work (a read-only folder, a file system without
/// locks) must not keep thinwire from starting. TDLib's own lock on its
/// database still refuses a second client.
fn start_without_lock(error: &std::io::Error) {
    tracing::warn!(%error, "single-instance lock unavailable; starting without it");
}

/// A wait on a background thread, so the UI thread does no file I/O.
struct Wait {
    path: PathBuf,
    result: Receiver<Result<InstanceLock, LockError>>,
}

impl Wait {
    fn spawn(path: PathBuf, deadline: Instant, ctx: &egui::Context) -> Self {
        let (tx, result) = std::sync::mpsc::channel();
        let wake = ctx.clone();
        let wait_path = path.clone();
        let spawned = std::thread::Builder::new()
            .name("thinwire-instance-wait".into())
            .spawn(move || {
                let _ = tx.send(InstanceLock::acquire_until(&wait_path, deadline));
                wake.request_repaint();
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "the instance wait did not start");
        }
        Self { path, result }
    }
}

/// What the window shows.
enum Stage {
    Waiting(Wait),
    Failed { path: PathBuf },
    Running(Box<ThinwireApp>),
}

/// The eframe app: the startup wait, then the thinwire window.
pub struct Shell {
    stage: Stage,
    /// Used once, when the main window starts.
    settings: Option<Settings>,
}

impl Shell {
    #[must_use]
    pub fn new(startup: Startup, settings: Settings, ctx: &egui::Context) -> Self {
        match startup {
            Startup::Ready => Self {
                stage: Stage::Running(Box::new(ThinwireApp::new(settings, ctx))),
                settings: None,
            },
            Startup::Waiting { path, deadline } => Self {
                stage: Stage::Waiting(Wait::spawn(path, deadline, ctx)),
                settings: Some(settings),
            },
        }
    }

    fn poll_wait(&mut self, ctx: &egui::Context) {
        let Stage::Waiting(wait) = &self.stage else {
            return;
        };
        let path = wait.path.clone();
        let result = match wait.result.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            // The thread did not start or died: treat it as held.
            Err(TryRecvError::Disconnected) => Err(LockError::Held),
        };
        match result {
            Ok(lock) => {
                hold(lock);
                self.start_main(ctx);
            }
            Err(LockError::Io(error)) => {
                start_without_lock(&error);
                self.start_main(ctx);
            }
            Err(LockError::Held) => {
                tracing::warn!("another thinwire still holds the lock; not starting");
                self.stage = Stage::Failed { path };
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(FAILED_SIZE.into()));
            }
        }
    }

    /// The old copy is gone: start the protocols and grow the window.
    fn start_main(&mut self, ctx: &egui::Context) {
        let settings = self.settings.take().unwrap_or_else(Settings::load);
        self.stage = Stage::Running(Box::new(ThinwireApp::new(settings, ctx)));
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(MAIN_SIZE.into()));
        ctx.request_repaint();
    }
}

impl eframe::App for Shell {
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        if let Stage::Running(app) = &mut self.stage {
            app.logic(ctx, frame);
            return;
        }
        self.poll_wait(ctx);
        if matches!(self.stage, Stage::Waiting(_)) {
            ctx.request_repaint_after(RETRY_STEP);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        match &mut self.stage {
            Stage::Running(app) => app.ui(ui, frame),
            Stage::Waiting(_) => waiting(ui),
            Stage::Failed { path } => match failed(ui, path) {
                Some(FailedChoice::TryAgain) => {
                    let path = path.clone();
                    let deadline = Instant::now() + LOCK_WAIT;
                    self.stage = Stage::Waiting(Wait::spawn(path, deadline, ui.ctx()));
                }
                Some(FailedChoice::Quit) => {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
                None => {}
            },
        }
    }

    fn on_exit(&mut self, gl: Option<&eframe::glow::Context>) {
        if let Stage::Running(app) = &mut self.stage {
            app.on_exit(gl);
        }
    }
}

/// The small "still closing" window.
pub(crate) fn waiting(ui: &mut egui::Ui) {
    egui::CentralPanel::default().show(ui, |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(48.0);
            ui.spinner();
            ui.add_space(12.0);
            ui.heading(CLOSING_NOTE);
            ui.add_space(4.0);
            ui.label("It starts when the window you closed has quit.");
        });
    });
}

/// A button on the failure window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailedChoice {
    TryAgain,
    Quit,
}

/// The lock is still held after [`LOCK_WAIT`]: what happened, why, and what
/// to do.
pub(crate) fn failed(ui: &mut egui::Ui, path: &Path) -> Option<FailedChoice> {
    let mut choice = None;
    egui::CentralPanel::default().show(ui, |ui| {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.heading(FAILED_TITLE);
            ui.add_space(6.0);
            ui.label(format!(
                "Another thinwire is still running, or did not finish closing within {:.1} seconds.",
                LOCK_WAIT.as_secs_f32()
            ));
            ui.label(
                "thinwire runs one copy at a time, so two copies never open the same chat data.",
            );
            ui.add_space(6.0);
            ui.label(concat!(
                "Close the other thinwire window, wait a few seconds, and try again. ",
                "If no thinwire window is open, end the thinwire process in your ",
                "system monitor, then try again.",
            ));
            ui.add_space(4.0);
            ui.small(format!("Lock file: {}", path.display()));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button(TRY_AGAIN).clicked() {
                    choice = Some(FailedChoice::TryAgain);
                }
                if ui.button(QUIT).clicked() {
                    choice = Some(FailedChoice::Quit);
                }
            });
        });
    });
    choice
}

#[cfg(test)]
mod tests {
    use egui::accesskit::Role;
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        std::env::temp_dir()
            .join(format!(
                "thinwire-startup-{name}-{}-{nanos}",
                std::process::id()
            ))
            .join("instance.lock")
    }

    #[test]
    fn the_wait_covers_the_longest_close_of_the_old_copy() {
        assert!(LOCK_WAIT >= SHUTDOWN_TIMEOUT + WATCHDOG_MARGIN);
        assert!(NOTE_AFTER < LOCK_WAIT);
        assert_eq!(NOTE_AFTER, Duration::from_secs(1));
    }

    #[test]
    fn a_held_lock_waits_with_the_small_window() {
        let path = scratch("held");
        let other = InstanceLock::try_acquire(&path).expect("the old copy");
        let launch = Instant::now();
        let startup = first_try(Some(path.clone()), launch);
        assert!(
            launch.elapsed() >= NOTE_AFTER,
            "a short wait shows no window"
        );
        let Startup::Waiting {
            path: waiting,
            deadline,
        } = &startup
        else {
            panic!("expected a wait, got {startup:?}");
        };
        assert_eq!(waiting, &path);
        assert_eq!(*deadline, launch + LOCK_WAIT);
        assert_eq!(startup.window_size(), SMALL_SIZE);
        drop(other);
        let _ = std::fs::remove_dir_all(path.parent().expect("dir"));
    }

    #[test]
    fn no_data_folder_starts_at_once() {
        let startup = first_try(None, Instant::now());
        assert!(matches!(startup, Startup::Ready));
        assert_eq!(startup.window_size(), MAIN_SIZE);
    }

    #[test]
    fn the_startup_lock_comes_before_any_protocol() {
        let main = include_str!("../main.rs");
        let lock = main.find("app::startup::first_try(").expect("lock first");
        let run = main.find("eframe::run_native(").expect("run");
        assert!(
            lock < run,
            "the lock is taken before the window and the core"
        );
        let shell = include_str!("startup.rs");
        let new = &shell[shell.find("pub fn new(startup").expect("new")..];
        let new = &new[..new.find("\n    }\n").expect("end")];
        let waiting = &new[new.find("Startup::Waiting").expect("waiting arm")..];
        assert!(
            !waiting.contains("ThinwireApp::new"),
            "no core while another copy holds the lock"
        );
    }

    #[test]
    fn the_wait_window_says_thinwire_is_still_closing() {
        let mut harness = Harness::builder()
            .with_size(egui::vec2(SMALL_SIZE[0], SMALL_SIZE[1]))
            .build_ui(waiting);
        harness.run_steps(2);
        harness.get_by_label(CLOSING_NOTE);
    }

    #[test]
    fn the_failure_window_explains_and_offers_try_again_and_quit() {
        let path = PathBuf::from("/tmp/thinwire/instance.lock");
        let mut harness = Harness::builder()
            .with_size(egui::vec2(FAILED_SIZE[0], FAILED_SIZE[1]))
            .build_ui_state(
                move |ui, choice: &mut Option<FailedChoice>| {
                    if let Some(clicked) = failed(ui, &path) {
                        *choice = Some(clicked);
                    }
                },
                None,
            );
        harness.run();
        harness.get_by_label(FAILED_TITLE);
        harness.get_by_label_contains("one copy at a time");
        harness.get_by_label_contains("end the thinwire process");
        harness
            .get_by_role_and_label(Role::Button, TRY_AGAIN)
            .click();
        harness.run();
        assert_eq!(*harness.state(), Some(FailedChoice::TryAgain));
        harness.get_by_role_and_label(Role::Button, QUIT).click();
        harness.run();
        assert_eq!(*harness.state(), Some(FailedChoice::Quit));
    }
}
