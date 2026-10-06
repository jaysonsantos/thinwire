//! OS desktop notifications for thinwire (#32).
//!
//! The core decides what to show (`thinwire_core::notify`). This crate
//! shows it. A thread owns the OS backend, so no OS call runs on the UI
//! thread. The crate has no egui dependency: a TUI can use it too.
//!
//! Backends (MIT or Apache-2.0):
//! - Linux: freedesktop notifications over D-Bus (pure-Rust zbus, through
//!   `notify-rust`). A click opens the chat. The newest message of a chat
//!   replaces its notification, and a dismiss closes it.
//! - Windows: WinRT toasts (`windows`). The same as Linux: one toast per
//!   chat, replace, dismiss, and click (#161).
//! - macOS: show only, through `notify-rust`. Click, replace, and dismiss
//!   are not wired yet.
//!
//! No log line holds a title, a sender, or message text. A failed OS call
//! waits and tries the next command; notifications turn off for the run
//! only after `MAX_FAILURES_IN_A_ROW` failures in a row.

use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

pub use pipe::{INBOX_LIMIT, MAX_DISMISS_TRIES, MAX_FAILURES_IN_A_ROW, RETRY_AFTER, RETRY_MAX};
pub use thinwire_core::notify::{Notification, NotifyCommand, NotifyKey};

mod pipe;
#[cfg(any(windows, test))]
mod tagged;
#[cfg(windows)]
mod winrt;
#[cfg(target_os = "linux")]
mod xdg;

use pipe::{Inbox, Next, Policy};

/// A failed OS call. Only a fixed kind, never message data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendError(pub &'static str);

/// Called with the chat of a clicked notification. It runs on a backend
/// thread: send an intent and wake the frontend, do no UI work in it.
pub type ClickFn = Arc<dyn Fn(NotifyKey) + Send + Sync>;

/// One OS notification service.
pub trait Backend: Send + 'static {
    fn show(&mut self, notification: &Notification) -> Result<(), BackendError>;

    /// Change a notification that still shows. It never shows a new one.
    /// The default does nothing: a backend that cannot replace a shown
    /// notification must not show a second one (#160 review).
    fn update(&mut self, _notification: &Notification) -> Result<(), BackendError> {
        Ok(())
    }

    fn dismiss(&mut self, key: &NotifyKey) -> Result<(), BackendError>;
}

/// State that the frontend and the thread share.
#[derive(Default)]
struct Shared {
    inbox: Inbox,
    /// The thread runs an OS call now.
    busy: bool,
    /// The `Notifier` is gone: the thread ends when the inbox is empty.
    closed: bool,
    /// `flush` calls that wait now. While one waits, a failure does not
    /// wait its backoff: at exit, the last commands and their retries go
    /// out inside the flush limit (#168 item 19).
    flushing: usize,
    /// Too many failures: the thread drops every command.
    off: bool,
}

struct Pipe {
    shared: Mutex<Shared>,
    changed: Condvar,
}

impl Pipe {
    fn lock(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Handle of the notification thread. Drop it to stop the thread.
pub struct Notifier {
    pipe: Arc<Pipe>,
}

impl Notifier {
    /// Start the platform backend on its own thread.
    pub fn spawn(on_click: impl Fn(NotifyKey) + Send + Sync + 'static) -> Self {
        Self::spawn_with(platform_backend, on_click)
    }

    /// Start `make(clicks)` on its own thread. Tests use a fake backend.
    pub fn spawn_with<B: Backend>(
        make: impl FnOnce(ClickFn) -> B + Send + 'static,
        on_click: impl Fn(NotifyKey) + Send + Sync + 'static,
    ) -> Self {
        let pipe = Arc::new(Pipe {
            shared: Mutex::new(Shared::default()),
            changed: Condvar::new(),
        });
        let clicks: ClickFn = Arc::new(on_click);
        let worker = Arc::clone(&pipe);
        let spawned = thread::Builder::new()
            .name("thinwire-notify".into())
            .spawn(move || run(&worker, make(clicks)));
        if let Err(error) = spawned {
            tracing::warn!(%error, "desktop notification thread did not start");
            pipe.lock().off = true;
        }
        Self { pipe }
    }

    /// Queue commands for the thread. Never blocks on the OS: it only takes
    /// a short lock.
    pub fn send(&self, commands: impl IntoIterator<Item = NotifyCommand>) {
        let mut shared = self.pipe.lock();
        if shared.off {
            return;
        }
        let mut any = false;
        for command in commands {
            shared.inbox.push(command);
            any = true;
        }
        drop(shared);
        if any {
            self.pipe.changed.notify_all();
        }
    }

    /// Wait at most `limit` until the thread sent every queued command to
    /// the OS. The app calls it at exit, so the last dismisses arrive.
    /// Returns `true` when the inbox is empty and no OS call runs.
    pub fn flush(&self, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        let mut shared = self.pipe.lock();
        shared.flushing += 1;
        // Wake the thread if it waits after a failure.
        self.pipe.changed.notify_all();
        let done = loop {
            if shared.off || (shared.inbox.is_empty() && !shared.busy) {
                break true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break false;
            }
            shared = self
                .pipe
                .changed
                .wait_timeout(shared, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        };
        shared.flushing -= 1;
        done
    }
}

impl Drop for Notifier {
    fn drop(&mut self) {
        self.pipe.lock().closed = true;
        self.pipe.changed.notify_all();
    }
}

/// The thread: take one command, run it on the OS backend, follow `Health`.
fn run(pipe: &Pipe, mut backend: impl Backend) {
    let mut policy = Policy::default();
    loop {
        let command = {
            let mut shared = pipe.lock();
            loop {
                if let Some(command) = shared.inbox.pop() {
                    shared.busy = true;
                    break command;
                }
                if shared.closed {
                    return;
                }
                shared = pipe
                    .changed
                    .wait(shared)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        };
        let (kind, result) = match &command {
            NotifyCommand::Show(notification) => ("show", backend.show(notification)),
            NotifyCommand::Update(notification) => ("update", backend.update(notification)),
            NotifyCommand::Dismiss(key) => ("dismiss", backend.dismiss(key)),
        };
        if result.is_ok() {
            tracing::debug!(kind, "desktop notification sent to the OS");
        }
        // A failed `Dismiss` goes back to the inbox, so its notification
        // does not stay on screen after one busy D-Bus call (#165). It goes
        // back before the wait, so a flush waits for it (#168 item 19).
        let (next, retry) = policy.record(&command, result);
        let mut shared = pipe.lock();
        shared.busy = false;
        if let Some(key) = retry
            && !shared.inbox.retry_dismiss(&key)
        {
            policy.forget(&key);
        }
        if next == Next::Off {
            shared.off = true;
            while shared.inbox.pop().is_some() {}
        }
        pipe.changed.notify_all();
        match next {
            Next::Go => {}
            Next::Wait(wait) => drop(back_off(pipe, shared, wait)),
            Next::Off => return,
        }
    }
}

/// Wait `wait` after a failed OS call. A flush ends the wait early, so the
/// app's short exit flush still sends the last commands (#168 item 19). The
/// end of the `Notifier` ends it too: nobody waits for the thread then.
fn back_off<'a>(
    pipe: &'a Pipe,
    mut shared: MutexGuard<'a, Shared>,
    wait: Duration,
) -> MutexGuard<'a, Shared> {
    let deadline = Instant::now() + wait;
    loop {
        if shared.flushing > 0 || shared.closed {
            return shared;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return shared;
        }
        shared = pipe
            .changed
            .wait_timeout(shared, left)
            .unwrap_or_else(PoisonError::into_inner)
            .0;
    }
}

#[cfg(target_os = "linux")]
fn platform_backend(clicks: ClickFn) -> impl Backend {
    xdg::Xdg::new(clicks)
}

#[cfg(windows)]
fn platform_backend(clicks: ClickFn) -> impl Backend {
    tagged::Tagged(winrt::Toasts::new(clicks))
}

#[cfg(target_os = "macos")]
fn platform_backend(_clicks: ClickFn) -> impl Backend {
    ShowOnly
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn platform_backend(_clicks: ClickFn) -> impl Backend {
    Unsupported
}

/// macOS: show a notification. No click, replace, or dismiss.
#[cfg(target_os = "macos")]
struct ShowOnly;

#[cfg(target_os = "macos")]
impl Backend for ShowOnly {
    fn show(&mut self, notification: &Notification) -> Result<(), BackendError> {
        notify_rust::Notification::new()
            .appname("thinwire")
            .summary(&notification.title)
            .body(&notification.body())
            .show()
            .map(drop)
            .map_err(|_| BackendError("show"))
    }

    fn dismiss(&mut self, _key: &NotifyKey) -> Result<(), BackendError> {
        Ok(())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
struct Unsupported;

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
impl Backend for Unsupported {
    fn show(&mut self, _notification: &Notification) -> Result<(), BackendError> {
        Err(BackendError("unsupported platform"))
    }

    fn dismiss(&mut self, _key: &NotifyKey) -> Result<(), BackendError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Duration;
    use thinwire_core::ProtocolId;

    #[derive(Debug, PartialEq, Eq)]
    enum Seen {
        Show(NotifyKey, u32),
        Dismiss(NotifyKey),
    }

    struct Fake {
        seen: Arc<Mutex<Vec<Seen>>>,
        clicks: ClickFn,
        fail_on_show: bool,
        /// Dismisses that fail before one works.
        fail_dismisses: u32,
    }

    impl Backend for Fake {
        fn show(&mut self, notification: &Notification) -> Result<(), BackendError> {
            if self.fail_on_show {
                return Err(BackendError("no notification server"));
            }
            self.seen
                .lock()
                .expect("seen")
                .push(Seen::Show(notification.key.clone(), notification.count));
            // A fake click on every shown notification.
            (self.clicks)(notification.key.clone());
            Ok(())
        }

        fn dismiss(&mut self, key: &NotifyKey) -> Result<(), BackendError> {
            if self.fail_dismisses > 0 {
                self.fail_dismisses -= 1;
                return Err(BackendError("close"));
            }
            self.seen
                .lock()
                .expect("seen")
                .push(Seen::Dismiss(key.clone()));
            Ok(())
        }
    }

    fn key(id: &str) -> NotifyKey {
        NotifyKey {
            protocol: ProtocolId::Telegram,
            conversation_id: id.into(),
        }
    }

    fn note(id: &str, count: u32) -> NotifyCommand {
        NotifyCommand::Show(Notification {
            key: key(id),
            title: "Ada".into(),
            sender: None,
            preview: "hi".into(),
            count,
        })
    }

    /// macOS show ignores the click callback. Windows reports a click only
    /// from `Activated`, which is a user click, not from `Show`.
    #[test]
    fn showing_a_notification_is_not_a_click_on_macos_or_windows() {
        let src = include_str!("lib.rs");
        assert!(
            src.contains("fn platform_backend(_clicks: ClickFn)"),
            "macOS show has no click callback"
        );
        let winrt = include_str!("winrt.rs");
        let post = &winrt[winrt.find("fn post(").expect("post")..];
        let post = &post[..post.find("fn has(").expect("has")];
        let activated = post.find("toast.Activated").expect("activated");
        let clicks = post.find("clicks(").expect("clicks");
        let show = post.find(".Show(").expect("show");
        assert!(
            activated < clicks && clicks < show,
            "Show does not report a click; Activated does"
        );
    }

    #[test]
    fn the_thread_runs_commands_in_order_and_reports_clicks() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let clicked = Arc::new(Mutex::new(Vec::new()));
        let fake_seen = Arc::clone(&seen);
        let clicked_by = Arc::clone(&clicked);
        let notifier = Notifier::spawn_with(
            move |clicks| Fake {
                seen: fake_seen,
                clicks,
                fail_on_show: false,
                fail_dismisses: 0,
            },
            move |key| clicked_by.lock().expect("clicked").push(key),
        );
        // One at a time: queued commands of one chat merge in the inbox.
        for command in [
            note("telegram:1", 1),
            note("telegram:1", 2),
            NotifyCommand::Dismiss(key("telegram:1")),
        ] {
            notifier.send([command]);
            assert!(notifier.flush(Duration::from_secs(2)));
        }
        assert_eq!(
            *seen.lock().expect("seen"),
            vec![
                Seen::Show(key("telegram:1"), 1),
                Seen::Show(key("telegram:1"), 2),
                Seen::Dismiss(key("telegram:1")),
            ]
        );
        assert_eq!(clicked.lock().expect("clicked").len(), 2);
    }

    #[test]
    fn one_failure_does_not_turn_notifications_off() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let fake_seen = Arc::clone(&seen);
        let notifier = Notifier::spawn_with(
            move |clicks| Fake {
                seen: fake_seen,
                clicks,
                fail_on_show: true,
                fail_dismisses: 0,
            },
            |_| {},
        );
        // The Show fails once; the next command still reaches the backend.
        notifier.send([
            note("telegram:1", 1),
            NotifyCommand::Dismiss(key("telegram:2")),
        ]);
        assert!(notifier.flush(RETRY_AFTER * 3), "the thread went on");
        assert_eq!(
            *seen.lock().expect("seen"),
            vec![Seen::Dismiss(key("telegram:2"))]
        );
    }

    #[test]
    fn a_backend_with_no_replace_shows_no_second_notification() {
        // The macOS backend keeps the default `update`: a hidden
        // text must not appear as a second notification (#160 review).
        let seen = Arc::new(Mutex::new(Vec::new()));
        let fake_seen = Arc::clone(&seen);
        let notifier = Notifier::spawn_with(
            move |clicks| Fake {
                seen: fake_seen,
                clicks,
                fail_on_show: false,
                fail_dismisses: 0,
            },
            |_| {},
        );
        notifier.send([note("telegram:1", 1)]);
        assert!(notifier.flush(Duration::from_secs(2)));
        let NotifyCommand::Show(shown) = note("telegram:1", 1) else {
            unreachable!()
        };
        notifier.send([NotifyCommand::Update(Notification {
            preview: "New message".into(),
            ..shown
        })]);
        assert!(notifier.flush(Duration::from_secs(2)));
        assert_eq!(
            *seen.lock().expect("seen"),
            vec![Seen::Show(key("telegram:1"), 1)],
            "one notification only"
        );
    }

    /// #168 item 19: at exit the app flushes for 500 ms, less than one
    /// failure wait. A flush ends that wait, so a dismiss that fails before
    /// or during the flush is tried again inside the limit.
    #[test]
    fn a_dismiss_that_fails_at_exit_is_tried_again_inside_the_flush() {
        let exit_limit = Duration::from_millis(500);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let fake_seen = Arc::clone(&seen);
        let notifier = Notifier::spawn_with(
            move |clicks| Fake {
                seen: fake_seen,
                clicks,
                fail_on_show: false,
                fail_dismisses: 2,
            },
            |_| {},
        );
        // The first try fails before the flush: the thread waits.
        notifier.send([NotifyCommand::Dismiss(key("telegram:1"))]);
        thread::sleep(RETRY_AFTER / 4);
        assert!(
            seen.lock().expect("seen").is_empty(),
            "with no flush, the retry waits"
        );
        // The second try fails during the flush; the third one works.
        assert!(notifier.flush(exit_limit), "the retries ran in time");
        assert_eq!(
            *seen.lock().expect("seen"),
            vec![Seen::Dismiss(key("telegram:1"))]
        );
    }

    #[test]
    fn a_failed_dismiss_is_tried_again() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let fake_seen = Arc::clone(&seen);
        let notifier = Notifier::spawn_with(
            move |clicks| Fake {
                seen: fake_seen,
                clicks,
                fail_on_show: false,
                fail_dismisses: 1,
            },
            |_| {},
        );
        notifier.send([NotifyCommand::Dismiss(key("telegram:1"))]);
        assert!(notifier.flush(RETRY_AFTER * 3), "the retry ran");
        assert_eq!(
            *seen.lock().expect("seen"),
            vec![Seen::Dismiss(key("telegram:1"))],
            "the second try reached the OS (#165)"
        );
    }

    #[test]
    fn a_dismiss_that_keeps_failing_stops_after_its_tries() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let fake_seen = Arc::clone(&seen);
        let notifier = Notifier::spawn_with(
            move |clicks| Fake {
                seen: fake_seen,
                clicks,
                fail_on_show: false,
                fail_dismisses: MAX_DISMISS_TRIES,
            },
            |_| {},
        );
        notifier.send([NotifyCommand::Dismiss(key("telegram:1"))]);
        // The flush ends each failure wait. After the last try the thread
        // gives up on this dismiss and goes idle.
        assert!(notifier.flush(RETRY_AFTER * 10));
        assert!(seen.lock().expect("seen").is_empty());
        // The thread is still on: the next command works.
        notifier.send([NotifyCommand::Dismiss(key("telegram:2"))]);
        assert!(notifier.flush(RETRY_AFTER * 6));
        assert_eq!(
            *seen.lock().expect("seen"),
            vec![Seen::Dismiss(key("telegram:2"))]
        );
    }

    #[test]
    fn flush_waits_for_the_last_commands() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let fake_seen = Arc::clone(&seen);
        let notifier = Notifier::spawn_with(
            move |clicks| Fake {
                seen: fake_seen,
                clicks,
                fail_on_show: false,
                fail_dismisses: 0,
            },
            |_| {},
        );
        notifier.send([NotifyCommand::Dismiss(key("telegram:1"))]);
        assert!(notifier.flush(Duration::from_secs(2)));
        assert_eq!(seen.lock().expect("seen").len(), 1);
    }

    #[test]
    fn no_log_line_holds_notification_text() {
        for src in [
            include_str!("lib.rs"),
            include_str!("xdg.rs"),
            include_str!("pipe.rs"),
            include_str!("tagged.rs"),
            include_str!("winrt.rs"),
        ] {
            for line in src.lines().filter(|line| line.contains("tracing::")) {
                for field in ["title", "body", "preview", "sender"] {
                    assert!(!line.contains(field), "{line}");
                }
            }
        }
    }
}
