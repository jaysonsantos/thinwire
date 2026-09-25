//! OS desktop notifications for thinwire (#32).
//!
//! The core decides what to show (`thinwire_core::notify`). This crate
//! shows it. A thread owns the OS backend, so no OS call runs on the UI
//! thread. The crate has no egui dependency: a TUI can use it too.
//!
//! Backends (all through `notify-rust`, MIT or Apache-2.0):
//! - Linux: freedesktop notifications over D-Bus (pure-Rust zbus). A click
//!   opens the chat. The newest message of a chat replaces its notification,
//!   and a dismiss closes it.
//! - macOS and Windows: show only. Click, replace, and dismiss are not
//!   wired yet.
//!
//! No log line holds a title, a sender, or message text. A backend error
//! logs its kind once, then notifications stay off for this run.

use std::sync::Arc;
use std::sync::mpsc::{self, Sender};
use std::thread;

pub use thinwire_core::notify::{Notification, NotifyCommand, NotifyKey};

#[cfg(target_os = "linux")]
mod xdg;

/// A failed OS call. Only a fixed kind, never message data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendError(pub &'static str);

/// Called with the chat of a clicked notification. It runs on a backend
/// thread: send an intent and wake the frontend, do no UI work in it.
pub type ClickFn = Arc<dyn Fn(NotifyKey) + Send + Sync>;

/// One OS notification service.
pub trait Backend: Send + 'static {
    fn show(&mut self, notification: &Notification) -> Result<(), BackendError>;
    fn dismiss(&mut self, key: &NotifyKey) -> Result<(), BackendError>;
}

/// Handle of the notification thread. Drop it to stop the thread.
pub struct Notifier {
    commands: Sender<NotifyCommand>,
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
        let (commands, inbox) = mpsc::channel::<NotifyCommand>();
        let clicks: ClickFn = Arc::new(on_click);
        let spawned = thread::Builder::new()
            .name("thinwire-notify".into())
            .spawn(move || {
                let mut backend = make(clicks);
                let mut on = true;
                for command in inbox {
                    if !on {
                        continue;
                    }
                    let result = match &command {
                        NotifyCommand::Show(notification) => backend.show(notification),
                        NotifyCommand::Dismiss(key) => backend.dismiss(key),
                    };
                    if result.is_ok() {
                        let kind = match &command {
                            NotifyCommand::Show(_) => "show",
                            NotifyCommand::Dismiss(_) => "dismiss",
                        };
                        tracing::debug!(kind, "desktop notification sent to the OS");
                    }
                    if let Err(BackendError(kind)) = result {
                        tracing::warn!(kind, "desktop notifications are off for this run");
                        on = false;
                    }
                }
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "desktop notification thread did not start");
        }
        Self { commands }
    }

    /// Queue commands for the thread. Never blocks.
    pub fn send(&self, commands: impl IntoIterator<Item = NotifyCommand>) {
        for command in commands {
            if self.commands.send(command).is_err() {
                return;
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn platform_backend(clicks: ClickFn) -> impl Backend {
    xdg::Xdg::new(clicks)
}

#[cfg(any(target_os = "macos", windows))]
fn platform_backend(_clicks: ClickFn) -> impl Backend {
    ShowOnly
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn platform_backend(_clicks: ClickFn) -> impl Backend {
    Unsupported
}

/// macOS and Windows: show a notification. No click, replace, or dismiss.
#[cfg(any(target_os = "macos", windows))]
struct ShowOnly;

#[cfg(any(target_os = "macos", windows))]
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

    fn wait_for(seen: &Arc<Mutex<Vec<Seen>>>, len: usize) {
        for _ in 0..200 {
            if seen.lock().expect("seen").len() >= len {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
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
            },
            move |key| clicked_by.lock().expect("clicked").push(key),
        );
        notifier.send([
            note("telegram:1", 1),
            note("telegram:1", 2),
            NotifyCommand::Dismiss(key("telegram:1")),
        ]);
        wait_for(&seen, 3);
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
    fn a_failing_backend_turns_off_after_one_error() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let fake_seen = Arc::clone(&seen);
        let notifier = Notifier::spawn_with(
            move |clicks| Fake {
                seen: fake_seen,
                clicks,
                fail_on_show: true,
            },
            |_| {},
        );
        notifier.send([
            note("telegram:1", 1),
            NotifyCommand::Dismiss(key("telegram:1")),
        ]);
        thread::sleep(Duration::from_millis(50));
        assert!(
            seen.lock().expect("seen").is_empty(),
            "no dismiss after the backend turned off"
        );
    }

    #[test]
    fn no_log_line_holds_notification_text() {
        for src in [include_str!("lib.rs"), include_str!("xdg.rs")] {
            for line in src.lines().filter(|line| line.contains("tracing::")) {
                for field in ["title", "body", "preview", "sender"] {
                    assert!(!line.contains(field), "{line}");
                }
            }
        }
    }
}
