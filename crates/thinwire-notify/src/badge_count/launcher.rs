//! The count on a Linux launcher: `com.canonical.Unity.LauncherEntry`.
//!
//! One thread owns the session bus connection and sends each signal. The
//! caller only queues a count. Counts that wait together become one
//! signal, with the newest count.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use zbus::zvariant::Value;

use crate::BackendError;

/// The launcher finds the entry by the desktop file of the app.
const APP_URI: &str = "application://thinwire.desktop";
const INTERFACE: &str = "com.canonical.Unity.LauncherEntry";
/// The path has no meaning for a launcher: it reads the signal of each path.
const OBJECT_PATH: &str = "/com/thinwire/LauncherEntry";
const SIGNAL: &str = "Update";

enum Message {
    Count(u32),
    /// Answer when each count before this message went out.
    Flush(Sender<()>),
}

/// Handle of the D-Bus thread. The thread starts with the first count
/// and ends when the handle drops.
#[derive(Default)]
pub(super) struct Launcher {
    queue: Option<Sender<Message>>,
}

impl Launcher {
    /// Queue `count` for the thread. It never blocks on the bus.
    pub(super) fn send(&mut self, count: u32) {
        if self.queue.is_none() {
            self.queue = start(SessionBus::default());
        }
        if let Some(queue) = &self.queue
            && queue.send(Message::Count(count)).is_err()
        {
            self.queue = None;
        }
    }

    /// Wait at most `limit` until the thread sent each queued count.
    pub(super) fn flush(&self, limit: Duration) {
        let Some(queue) = &self.queue else {
            return;
        };
        let (done, wait) = mpsc::channel();
        if queue.send(Message::Flush(done)).is_ok() {
            let _ = wait.recv_timeout(limit);
        }
    }
}

/// Sends one count to the launcher.
trait Emit: Send + 'static {
    fn emit(&mut self, count: u32) -> Result<(), BackendError>;
}

fn start(emit: impl Emit) -> Option<Sender<Message>> {
    let (queue, messages) = mpsc::channel();
    let spawned = thread::Builder::new()
        .name("thinwire-badge".into())
        .spawn(move || run(&messages, emit));
    match spawned {
        Ok(_) => Some(queue),
        Err(error) => {
            tracing::warn!(%error, "launcher badge thread did not start");
            None
        }
    }
}

/// Send the newest count of each group of messages that wait together.
fn run(messages: &Receiver<Message>, mut emit: impl Emit) {
    while let Ok(first) = messages.recv() {
        let mut count = None;
        let mut waiters = Vec::new();
        let mut next = Some(first);
        while let Some(message) = next {
            match message {
                Message::Count(newest) => count = Some(newest),
                Message::Flush(done) => waiters.push(done),
            }
            next = messages.try_recv().ok();
        }
        if let Some(count) = count
            && let Err(BackendError(kind)) = emit.emit(count)
        {
            // No session bus, for example. The next count tries again.
            tracing::debug!(kind, "launcher badge not sent");
        }
        for done in waiters {
            let _ = done.send(());
        }
    }
}

/// The body of the `Update` signal: the app and its properties.
fn update_body(count: u32) -> (&'static str, HashMap<&'static str, Value<'static>>) {
    let properties = HashMap::from([
        ("count", Value::I64(i64::from(count))),
        ("count-visible", Value::Bool(count > 0)),
    ]);
    (APP_URI, properties)
}

/// The session bus. The connection opens with the first signal.
#[derive(Default)]
struct SessionBus {
    bus: Option<zbus::blocking::Connection>,
}

impl Emit for SessionBus {
    fn emit(&mut self, count: u32) -> Result<(), BackendError> {
        let bus = match self.bus.take() {
            Some(bus) => bus,
            None => {
                zbus::blocking::Connection::session().map_err(|_| BackendError("session bus"))?
            }
        };
        // A failure drops the connection: the next count makes a new one.
        bus.emit_signal(
            None::<&str>,
            OBJECT_PATH,
            INTERFACE,
            SIGNAL,
            &update_body(count),
        )
        .map_err(|_| BackendError("launcher update"))?;
        self.bus = Some(bus);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone, Default)]
    struct Sent(Arc<Mutex<Vec<u32>>>);

    impl Emit for Sent {
        fn emit(&mut self, count: u32) -> Result<(), BackendError> {
            self.0.lock().expect("lock").push(count);
            Ok(())
        }
    }

    #[test]
    fn the_signal_has_the_count_and_shows_it_only_over_zero() {
        let (app, properties) = update_body(3);
        assert_eq!(app, "application://thinwire.desktop");
        assert_eq!(properties.get("count"), Some(&Value::I64(3)));
        assert_eq!(properties.get("count-visible"), Some(&Value::Bool(true)));
        let (_, properties) = update_body(0);
        assert_eq!(properties.get("count"), Some(&Value::I64(0)));
        assert_eq!(properties.get("count-visible"), Some(&Value::Bool(false)));
        assert_eq!(properties.len(), 2);
    }

    #[test]
    fn counts_that_wait_together_become_one_signal() {
        let sent = Sent::default();
        let (queue, messages) = mpsc::channel();
        let (done, wait) = mpsc::channel();
        for message in [
            Message::Count(1),
            Message::Count(2),
            Message::Count(5),
            Message::Flush(done),
        ] {
            queue.send(message).expect("queue");
        }
        drop(queue);
        run(&messages, sent.clone());
        assert_eq!(*sent.0.lock().expect("lock"), [5]);
        wait.try_recv().expect("the flush has its answer");
    }

    #[test]
    fn a_flush_waits_for_the_thread_and_a_failed_send_does_not_stop_it() {
        struct FailFirst(Sent, bool);
        impl Emit for FailFirst {
            fn emit(&mut self, count: u32) -> Result<(), BackendError> {
                if std::mem::replace(&mut self.1, false) {
                    return Err(BackendError("test"));
                }
                self.0.emit(count)
            }
        }
        let sent = Sent::default();
        let launcher = Launcher {
            queue: start(FailFirst(sent.clone(), true)),
        };
        let queue = launcher.queue.as_ref().expect("thread");
        queue.send(Message::Count(4)).expect("queue");
        launcher.flush(Duration::from_secs(10));
        queue.send(Message::Count(0)).expect("queue");
        launcher.flush(Duration::from_secs(10));
        assert_eq!(*sent.0.lock().expect("lock"), [0]);
    }
}
