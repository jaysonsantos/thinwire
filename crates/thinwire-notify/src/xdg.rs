//! Linux: freedesktop notifications over D-Bus.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;

use crate::{Backend, BackendError, ClickFn, Notification, NotifyKey};

const APP_NAME: &str = "thinwire";
const OPEN_ACTION: &str = "default";

/// Server ids of the shown notifications, one per chat.
pub(crate) struct Xdg {
    ids: HashMap<NotifyKey, u32>,
    /// Chat of each server id, for the click waiters.
    keys: Arc<Mutex<HashMap<u32, NotifyKey>>>,
    /// Server ids with a click waiter. A replaced notification keeps its id,
    /// so its waiter stays and no second one starts.
    waiting: Arc<Mutex<HashSet<u32>>>,
    clicks: ClickFn,
}

impl Xdg {
    pub(crate) fn new(clicks: ClickFn) -> Self {
        Self {
            ids: HashMap::new(),
            keys: Arc::new(Mutex::new(HashMap::new())),
            waiting: Arc::new(Mutex::new(HashSet::new())),
            clicks,
        }
    }
}

impl Backend for Xdg {
    fn show(&mut self, notification: &Notification) -> Result<(), BackendError> {
        let mut message = notify_rust::Notification::new();
        message
            .appname(APP_NAME)
            .summary(&notification.title)
            .body(&notification.body())
            .action(OPEN_ACTION, "Open");
        if let Some(id) = self.ids.get(&notification.key) {
            message.id(*id);
        }
        let handle = message.show().map_err(|_| BackendError("show"))?;
        let id = handle.id();
        self.ids.insert(notification.key.clone(), id);
        lock(&self.keys).insert(id, notification.key.clone());
        if !lock(&self.waiting).insert(id) {
            return Ok(());
        }
        let keys = Arc::clone(&self.keys);
        let waiting = Arc::clone(&self.waiting);
        let clicks = Arc::clone(&self.clicks);
        let spawned = thread::Builder::new()
            .name("thinwire-notify-click".into())
            .spawn(move || {
                // Returns on a click, or when the notification closes.
                handle.wait_for_action(|action| {
                    if action == OPEN_ACTION
                        && let Some(key) = lock(&keys).get(&id).cloned()
                    {
                        clicks(key);
                    }
                });
                lock(&waiting).remove(&id);
            });
        if spawned.is_err() {
            lock(&self.waiting).remove(&id);
        }
        Ok(())
    }

    fn dismiss(&mut self, key: &NotifyKey) -> Result<(), BackendError> {
        let Some(id) = self.ids.remove(key) else {
            return Ok(());
        };
        lock(&self.keys).remove(&id);
        close_notification(id)
    }
}

/// `CloseNotification` by id. The click waiter owns the handle.
fn close_notification(id: u32) -> Result<(), BackendError> {
    let connection =
        zbus::blocking::Connection::session().map_err(|_| BackendError("session bus"))?;
    connection
        .call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "CloseNotification",
            &(id,),
        )
        .map(drop)
        .map_err(|_| BackendError("close"))
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
