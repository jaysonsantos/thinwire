//! Startup: take the single-instance lock before any protocol starts.
//!
//! A new launch right after a close often finds the old process still
//! closing its clients (the window hides at once, the process stays up
//! through the close limit and the watchdog's last sleep). So a held lock
//! means "wait": try again until [`LOCK_WAIT`]. A short wait shows no window.
//! After [`NOTE_AFTER`] a small
//! window says that thinwire is still closing. If the lock is still held at
//! the end, the window explains what happened instead of starting a second
//! copy.
//!
//! A held lock whose process is running, not closing, is different. This
//! launch asks the endpoint next to the lock to show that window, and it
//! asks again while the lock stays held. The last ask of the silent wait
//! runs before the small window opens. The last ask of the long wait runs
//! before the failure window. When the running instance accepts, this
//! process exits and that window comes forward. A closing instance, a stale
//! endpoint, or no answer keeps the wait above.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use eframe::egui;
use thinwire_core::instance::{InstanceLock, LockError, RETRY_STEP, ShowListener, ShowReply, ask};

use super::{
    SHUTDOWN_TIMEOUT, Settings, ThinwireApp, WATCHDOG_LOCK_HOLD, motion, reduce_motion, theme,
};

/// Slack past the old process's worst-case exit. Covers the time from the
/// user launching this process until the wait starts, and the OS dropping
/// the lock after `process::exit`.
const LOCK_START_SLACK: Duration = Duration::from_millis(250);

/// How long a new launch waits for the old one to quit.
///
/// The old process holds the lock until the watchdog exits, which is
/// [`SHUTDOWN_TIMEOUT`] plus [`WATCHDOG_LOCK_HOLD`] after the close
/// ([`WATCHDOG_MARGIN`](super::WATCHDOG_MARGIN) and then [`EXIT_FLOOR`](super::EXIT_FLOOR)).
/// [`LOCK_START_SLACK`] keeps a launch that starts during that exit from
/// giving up first.
pub const LOCK_WAIT: Duration = SHUTDOWN_TIMEOUT
    .saturating_add(WATCHDOG_LOCK_HOLD)
    .saturating_add(LOCK_START_SLACK);
const _: () = assert!(
    LOCK_WAIT.as_millis()
        >= SHUTDOWN_TIMEOUT.as_millis()
            + WATCHDOG_LOCK_HOLD.as_millis()
            + LOCK_START_SLACK.as_millis()
);
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
/// What happened, why, and what to do. The tool that ends a process has a
/// different name on each platform.
#[cfg(target_os = "macos")]
pub(crate) const FAILED_HELP: &str = "thinwire is still closing from last time. It usually takes a \
    few seconds. Try again, or quit the old thinwire in Activity Monitor.";
#[cfg(windows)]
pub(crate) const FAILED_HELP: &str = "thinwire is still closing from last time. It usually takes a \
    few seconds. Try again, or quit the old thinwire in Task Manager.";
#[cfg(not(any(target_os = "macos", windows)))]
pub(crate) const FAILED_HELP: &str = "thinwire is still closing from last time. It usually takes a \
    few seconds. Try again, or quit the old thinwire in your system monitor.";
pub(crate) const TRY_AGAIN: &str = "Try again";
pub(crate) const QUIT: &str = "Quit";

/// The lock of this process. A static is never dropped, so the lock stays
/// until the process ends and the OS releases it.
static HELD: OnceLock<InstanceLock> = OnceLock::new();
/// The show listener. It stays up while the process closes, so a second
/// launch hears "closing" instead of bringing the hidden window back.
static SHOW: OnceLock<ShowListener> = OnceLock::new();

fn hold(lock: InstanceLock) {
    tracing::debug!(path = %lock.path().display(), "single-instance lock taken");
    let path = lock.path().to_path_buf();
    if HELD.set(lock).is_err() {
        return;
    }
    match ShowListener::spawn(&path) {
        Ok(listener) => {
            let _ = SHOW.set(listener);
        }
        Err(error) => {
            tracing::warn!(%error, "a second launch cannot bring this window forward");
        }
    }
}

/// Wake the UI when a second launch asks to show the window.
pub(crate) fn listen_for_show(wake: impl Fn() + Send + 'static) {
    if let Some(listener) = SHOW.get() {
        listener.set_wake(wake);
    }
}

/// The window is hidden for shutdown. A second launch must keep waiting.
pub(crate) fn mark_closing() {
    if let Some(listener) = SHOW.get() {
        listener.mark_closing();
    }
}

/// `true` once for each accepted show since the last call.
#[must_use]
pub(crate) fn take_show() -> bool {
    SHOW.get().is_some_and(ShowListener::take_show)
}

/// Where startup is after the first short wait.
#[derive(Debug)]
pub enum Startup {
    /// The lock is ours, or this platform cannot lock: start now.
    Ready,
    /// Another process holds the lock: wait with a small window.
    Waiting { path: PathBuf, deadline: Instant },
}

/// The result of the launch-time lock check.
#[derive(Debug)]
pub enum Launch {
    /// Open the window. `Ready` starts the app. `Waiting` shows the small
    /// window until the lock is free or the wait ends.
    Start(Startup),
    /// A running instance accepted the show. Exit without a window.
    Shown,
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
///
/// When the lock is already held, ask that process to show its window.
/// Ask again while the lock stays held. One more ask runs before this
/// opens the wait window. The UI thread does not exist yet.
#[must_use]
pub fn first_try(path: Option<PathBuf>, launch: Instant) -> Launch {
    let Some(path) = path else {
        tracing::warn!("no data folder: starting without the single-instance lock");
        return Launch::Start(Startup::Ready);
    };
    match InstanceLock::try_acquire(&path) {
        Ok(lock) => {
            hold(lock);
            Launch::Start(Startup::Ready)
        }
        Err(LockError::Io(error)) => {
            start_without_lock(&error);
            Launch::Start(Startup::Ready)
        }
        Err(LockError::Held) => match poll_held(&path, launch + NOTE_AFTER) {
            LockWait::Shown => {
                tracing::info!("the running thinwire will show its window");
                Launch::Shown
            }
            LockWait::Started(lock) => {
                hold(lock);
                Launch::Start(Startup::Ready)
            }
            LockWait::Unavailable(error) => {
                start_without_lock(&error);
                Launch::Start(Startup::Ready)
            }
            LockWait::StillHeld { closing } => {
                if closing {
                    tracing::info!("the other thinwire is closing; waiting for it to quit");
                } else {
                    tracing::info!("another thinwire holds the lock; waiting for it to quit");
                }
                Launch::Start(Startup::Waiting {
                    path,
                    deadline: launch + LOCK_WAIT,
                })
            }
        },
    }
}

/// What a wait on a held lock found.
#[derive(Debug)]
enum LockWait {
    /// The lock is ours.
    Started(InstanceLock),
    /// The running instance accepted the show.
    Shown,
    /// The lock was still held at the deadline.
    StillHeld { closing: bool },
    /// The lock file could not be used.
    Unavailable(std::io::Error),
}

/// Ask for the window while `path` stays locked, until `deadline`.
///
/// The call at `deadline` is the last one. It runs before the caller opens
/// the wait window or the failure window. A `closing` answer stops the
/// asks and waits only for the lock. Blocks the caller.
fn poll_held(path: &std::path::Path, deadline: Instant) -> LockWait {
    loop {
        let last = Instant::now() >= deadline;
        match ask(path) {
            ShowReply::Shown => return LockWait::Shown,
            ShowReply::Closing => {
                return match InstanceLock::acquire_until(path, deadline) {
                    Ok(lock) => LockWait::Started(lock),
                    Err(LockError::Held) => LockWait::StillHeld { closing: true },
                    Err(LockError::Io(error)) => LockWait::Unavailable(error),
                };
            }
            ShowReply::Unanswered if last => {
                return match InstanceLock::try_acquire(path) {
                    Ok(lock) => LockWait::Started(lock),
                    Err(LockError::Io(error)) => LockWait::Unavailable(error),
                    Err(LockError::Held) => LockWait::StillHeld { closing: false },
                };
            }
            ShowReply::Unanswered => {}
        }
        match InstanceLock::try_acquire(path) {
            Ok(lock) => return LockWait::Started(lock),
            Err(LockError::Io(error)) => return LockWait::Unavailable(error),
            Err(LockError::Held) => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    // The ask above is the one before the window opens or the
                    // wait fails. The deadline passed during that ask.
                    return LockWait::StillHeld { closing: false };
                }
                std::thread::sleep(RETRY_STEP.min(left));
            }
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
///
/// The thread also asks the running process to show its window. The last
/// ask runs before a held lock becomes the failure window.
struct Wait {
    path: PathBuf,
    result: Receiver<LockWait>,
}

impl Wait {
    fn spawn(path: PathBuf, deadline: Instant, ctx: &egui::Context) -> Self {
        let (tx, result) = std::sync::mpsc::channel();
        let wake = ctx.clone();
        let wait_path = path.clone();
        let spawned = std::thread::Builder::new()
            .name("thinwire-instance-wait".into())
            .spawn(move || {
                let _ = tx.send(poll_held(&wait_path, deadline));
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
    Failed {
        path: PathBuf,
    },
    Running(Box<ThinwireApp>),
    /// The running instance accepted the show. Close and exit.
    Shown,
}

/// The eframe app: the startup wait, then the thinwire window.
pub struct Shell {
    stage: Stage,
    /// Used once, when the main window starts.
    settings: Option<Settings>,
    /// OS reduce-motion. One watch for the whole window (#247).
    os_motion: reduce_motion::OsReduceMotion,
    /// Last value applied to egui, so a change repaints at once.
    last_reduce_motion: Option<bool>,
}

impl Shell {
    #[must_use]
    pub fn new(startup: Startup, settings: Settings, ctx: &egui::Context) -> Self {
        let os_motion = reduce_motion::OsReduceMotion::start(ctx.clone());
        let mut last_reduce_motion = None;
        motion::follow(ctx, &mut last_reduce_motion, os_motion.reduced());
        match startup {
            Startup::Ready => Self {
                stage: Stage::Running(Box::new(ThinwireApp::new(settings, ctx))),
                settings: None,
                os_motion,
                last_reduce_motion,
            },
            Startup::Waiting { path, deadline } => Self {
                stage: Stage::Waiting(Wait::spawn(path, deadline, ctx)),
                settings: Some(settings),
                os_motion,
                last_reduce_motion,
            },
        }
    }

    fn poll_wait(&mut self, ctx: &egui::Context) {
        let (path, recv) = {
            let Stage::Waiting(wait) = &self.stage else {
                return;
            };
            (wait.path.clone(), wait.result.try_recv())
        };
        let result = match recv {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                // The wait thread did not start or died. That is not a lock
                // timeout: say so, then show the same failure window.
                tracing::error!(
                    "the instance wait thread ended without a result; not starting a second copy"
                );
                self.stage = Stage::Failed { path };
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(FAILED_SIZE.into()));
                return;
            }
        };
        match result {
            LockWait::Started(lock) => {
                hold(lock);
                self.start_main(ctx);
            }
            LockWait::Unavailable(error) => {
                start_without_lock(&error);
                self.start_main(ctx);
            }
            LockWait::Shown => {
                tracing::info!("the running thinwire will show its window");
                self.stage = Stage::Shown;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            LockWait::StillHeld { .. } => {
                tracing::warn!(
                    path = %path.display(),
                    "another thinwire still holds the lock; not starting"
                );
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
        if motion::follow(ctx, &mut self.last_reduce_motion, self.os_motion.reduced()) {
            ctx.request_repaint();
        }
        if matches!(self.stage, Stage::Shown) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
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
            Stage::Shown => {}
            Stage::Failed { path } => match failed(ui) {
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
            motion::spinner(ui);
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
/// to do. Try again is the main action, Quit the second one.
pub(crate) fn failed(ui: &mut egui::Ui) -> Option<FailedChoice> {
    let mut choice = None;
    egui::CentralPanel::default().show(ui, |ui| {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.heading(FAILED_TITLE);
            ui.add_space(6.0);
            ui.label(FAILED_HELP);
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                let palette = theme::palette(ui);
                let try_again =
                    egui::Button::new(egui::RichText::new(TRY_AGAIN).color(palette.on_accent))
                        .fill(palette.accent);
                if ui.add(try_again).clicked() {
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

    /// A lock whose `instance.show` path is one byte past [`MAX_SOCKET_PATH_BYTES`].
    #[cfg(unix)]
    fn overlong_lock_path() -> PathBuf {
        let root = std::path::Path::new("/tmp");
        let name = "instance.show";
        let total = thinwire_core::instance::MAX_SOCKET_PATH_BYTES + 1;
        let overhead = root.as_os_str().len() + 1 + 1 + name.len();
        let socket = root.join("t".repeat(total - overhead)).join(name);
        assert_eq!(socket.as_os_str().len(), total);
        socket.with_file_name("instance.lock")
    }

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
        assert!(LOCK_WAIT >= SHUTDOWN_TIMEOUT + WATCHDOG_LOCK_HOLD + LOCK_START_SLACK);
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
        let Launch::Start(waiting) = &startup else {
            panic!("expected a wait, got {startup:?}");
        };
        let Startup::Waiting {
            path: waiting_path,
            deadline,
        } = waiting
        else {
            panic!("expected a wait, got {waiting:?}");
        };
        assert_eq!(waiting_path, &path);
        assert_eq!(*deadline, launch + LOCK_WAIT);
        assert_eq!(waiting.window_size(), SMALL_SIZE);
        drop(other);
        let _ = std::fs::remove_dir_all(path.parent().expect("dir"));
    }

    #[test]
    fn no_data_folder_starts_at_once() {
        let Launch::Start(startup) = first_try(None, Instant::now()) else {
            panic!("no data folder starts");
        };
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
        let between = &main[lock..run];
        assert!(
            between.contains("Launch::Shown"),
            "a shown window exits before this process opens one"
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

    /// #247: every frame reads the OS reduce-motion value, including while
    /// the startup window is the only window.
    #[test]
    fn the_shell_applies_os_reduce_motion_on_each_frame() {
        let shell = include_str!("startup.rs");
        let logic = &shell[shell.find("fn logic(").expect("logic")..];
        let logic = &logic[..logic.find("\n    fn ui(").expect("ui")];
        let follow = logic.find("motion::follow(").expect("follow");
        let running = logic.find("Stage::Running").expect("running");
        assert!(
            follow < running,
            "reduce-motion is applied before the main window draws"
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
        let mut harness = Harness::builder()
            .with_size(egui::vec2(FAILED_SIZE[0], FAILED_SIZE[1]))
            .build_ui_state(
                |ui, choice: &mut Option<FailedChoice>| {
                    if let Some(clicked) = failed(ui) {
                        *choice = Some(clicked);
                    }
                },
                None,
            );
        harness.run();
        harness.get_by_label(FAILED_TITLE);
        // What happened, why, and what to do.
        harness.get_by_label_contains("is still closing from last time");
        harness.get_by_label_contains("usually takes a few seconds");
        harness.get_by_label_contains("Try again, or quit the old thinwire in");
        harness
            .get_by_role_and_label(Role::Button, TRY_AGAIN)
            .click();
        harness.run();
        assert_eq!(*harness.state(), Some(FailedChoice::TryAgain));
        harness.get_by_role_and_label(Role::Button, QUIT).click();
        harness.run();
        assert_eq!(*harness.state(), Some(FailedChoice::Quit));
    }

    #[test]
    fn the_failure_text_names_the_process_tool_of_this_platform() {
        let tool = if cfg!(target_os = "macos") {
            "in Activity Monitor."
        } else if cfg!(windows) {
            "in Task Manager."
        } else {
            "in your system monitor."
        };
        assert!(FAILED_HELP.ends_with(tool), "{FAILED_HELP}");
        assert!(FAILED_HELP.starts_with(
            "thinwire is still closing from last time. It usually takes a few seconds. \
            Try again, or quit the old thinwire "
        ));
    }

    /// Try again comes first and is filled, Quit second.
    #[test]
    fn try_again_is_the_main_action() {
        let mut harness = Harness::builder()
            .with_size(egui::vec2(FAILED_SIZE[0], FAILED_SIZE[1]))
            .build_ui(|ui| {
                let _ = failed(ui);
            });
        harness.run();
        let try_again = harness
            .get_by_role_and_label(Role::Button, TRY_AGAIN)
            .rect();
        let quit = harness.get_by_role_and_label(Role::Button, QUIT).rect();
        assert!(try_again.max.x <= quit.min.x, "Try again left of Quit");
    }

    #[test]
    fn a_taken_lock_listens_for_a_second_launch() {
        let shell = include_str!("startup.rs");
        let hold = &shell[shell.find("fn hold(").expect("hold")..];
        let hold = &hold[..hold.find("\nfn ").expect("next")];
        assert!(hold.contains("ShowListener::spawn"));
    }

    #[test]
    fn a_running_instance_ends_the_second_launch() {
        let path = scratch("shown");
        let _other = InstanceLock::try_acquire(&path).expect("the running copy");
        let listener = ShowListener::spawn(&path).expect("show endpoint");
        let launch = Instant::now();
        let startup = first_try(Some(path.clone()), launch);
        assert!(
            matches!(startup, Launch::Shown),
            "the second launch exits, got {startup:?}"
        );
        assert!(
            launch.elapsed() < NOTE_AFTER,
            "an answer does not wait out the closing window"
        );
        assert!(listener.take_show());
        let _ = std::fs::remove_dir_all(path.parent().expect("dir"));
    }

    #[test]
    fn a_closing_instance_still_waits_for_the_lock() {
        let path = scratch("closing");
        let _other = InstanceLock::try_acquire(&path).expect("the closing copy");
        let listener = ShowListener::spawn(&path).expect("show endpoint");
        listener.mark_closing();
        let launch = Instant::now();
        let startup = first_try(Some(path.clone()), launch);
        assert!(
            launch.elapsed() >= NOTE_AFTER,
            "a closing copy gets the same wait as before"
        );
        assert!(
            matches!(startup, Launch::Start(Startup::Waiting { .. })),
            "got {startup:?}"
        );
        assert!(!listener.take_show(), "a closing window does not show");
        let _ = std::fs::remove_dir_all(path.parent().expect("dir"));
    }

    #[test]
    fn a_stale_show_socket_still_waits_for_the_lock() {
        let path = scratch("stale-show");
        let _other = InstanceLock::try_acquire(&path).expect("the old copy");
        leave_stale_socket(&thinwire_core::instance::socket_path(&path));
        let launch = Instant::now();
        let startup = first_try(Some(path.clone()), launch);
        assert!(
            matches!(startup, Launch::Start(Startup::Waiting { .. })),
            "a stale endpoint falls back to the lock, got {startup:?}"
        );
        assert!(launch.elapsed() >= NOTE_AFTER);
        let _ = std::fs::remove_dir_all(path.parent().expect("dir"));
    }

    /// The listener can bind after the first ask. The old code asked once,
    /// then only retried the lock, and the wait ended on the failure window.
    #[test]
    fn a_listener_that_binds_late_still_ends_the_second_launch() {
        let path = scratch("late-listener");
        let _other = InstanceLock::try_acquire(&path).expect("the running copy");
        let bind_path = path.clone();
        let binder = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(350));
            ShowListener::spawn(&bind_path).expect("late listener")
        });
        let launch = Instant::now();
        let startup = first_try(Some(path.clone()), launch);
        assert!(
            matches!(startup, Launch::Shown),
            "a late listener still brings the window forward, got {startup:?}"
        );
        assert!(
            launch.elapsed() >= Duration::from_millis(300),
            "the first ask found no listener, waited {:?}",
            launch.elapsed()
        );
        assert!(
            launch.elapsed() < NOTE_AFTER,
            "the show lands before the wait window, waited {:?}",
            launch.elapsed()
        );
        let listener = binder.join().expect("binder");
        assert!(listener.take_show());
        let _ = std::fs::remove_dir_all(path.parent().expect("dir"));
    }

    /// The long wait uses the same asks. A listener that binds before the
    /// deadline answers, so the caller does not open the failure window.
    #[test]
    fn a_show_is_asked_again_before_the_wait_fails() {
        let path = scratch("before-fail");
        let _other = InstanceLock::try_acquire(&path).expect("the running copy");
        let bind_path = path.clone();
        let binder = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(250));
            ShowListener::spawn(&bind_path).expect("late listener")
        });
        let start = Instant::now();
        let deadline = start + Duration::from_millis(700);
        let result = poll_held(&path, deadline);
        assert!(
            matches!(result, LockWait::Shown),
            "the last asks still reach the listener, got {result:?}"
        );
        assert!(
            start.elapsed()
                < deadline.saturating_duration_since(start) + Duration::from_millis(400),
            "waited {:?}",
            start.elapsed()
        );
        let listener = binder.join().expect("binder");
        assert!(listener.take_show());
        let _ = std::fs::remove_dir_all(path.parent().expect("dir"));
    }

    /// Bind fails before `listen`. The process still takes the lock and starts.
    /// A second launch finds no endpoint and waits, as it does with no listener.
    #[cfg(unix)]
    #[test]
    fn an_overlong_socket_path_still_starts_this_process() {
        let path = overlong_lock_path();
        let launch = Instant::now();
        let startup = first_try(Some(path.clone()), launch);
        assert!(
            matches!(startup, Launch::Start(Startup::Ready)),
            "a path that cannot bind still starts, got {startup:?}"
        );
        assert!(
            launch.elapsed() < Duration::from_millis(500),
            "no wait window, waited {:?}",
            launch.elapsed()
        );
        let socket = thinwire_core::instance::socket_path(&path);
        assert!(socket.as_os_str().len() > thinwire_core::instance::MAX_SOCKET_PATH_BYTES);
        assert!(
            socket.symlink_metadata().is_err(),
            "an overlong path must not leave a socket"
        );
        assert!(
            matches!(InstanceLock::try_acquire(&path), Err(LockError::Held)),
            "this process kept the lock"
        );
        if let Some(dir) = path.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    fn leave_stale_socket(path: &std::path::Path) {
        #[cfg(unix)]
        {
            use std::os::unix::net::UnixListener;
            drop(UnixListener::bind(path).expect("stale socket"));
        }
        #[cfg(not(unix))]
        {
            // A Windows pipe disappears with its process. No leftover file
            // means the show request finds nothing and the lock wait runs.
            let _ = path;
        }
    }
}
