//! Linux: freedesktop notifications over D-Bus.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;

use notify_rust::{Hint, Urgency};

use crate::{Backend, BackendError, ClickFn, Notification, NotifyKey};

const APP_NAME: &str = "thinwire";
/// `desktop-entry` hint: the `.desktop` file name without the suffix.
const DESKTOP_ENTRY: &str = "thinwire";
const OPEN_ACTION: &str = "default";
/// `replaces_id` of a new notification (freedesktop spec).
const NEW_NOTIFICATION: u32 = 0;

/// Server ids that the server returned to this process, one per chat.
///
/// A replace uses only an id from this book. Plasma drops a `Notify` that
/// replaces an unknown or expired id, so nothing shows (L6 live test). The
/// book starts empty at each start, and a closed notification leaves it.
#[derive(Debug, Default)]
pub(crate) struct IdBook {
    by_key: HashMap<NotifyKey, u32>,
    by_id: HashMap<u32, NotifyKey>,
}

impl IdBook {
    /// `replaces_id` for the next notification of `key`: the id that is
    /// still open for this chat, or `NEW_NOTIFICATION`.
    pub(crate) fn replace_id(&self, key: &NotifyKey) -> u32 {
        self.by_key.get(key).copied().unwrap_or(NEW_NOTIFICATION)
    }

    /// The server showed `key` with `id`.
    pub(crate) fn shown(&mut self, key: &NotifyKey, id: u32) {
        if let Some(old) = self.by_key.insert(key.clone(), id)
            && old != id
        {
            self.by_id.remove(&old);
        }
        self.by_id.insert(id, key.clone());
    }

    /// The notification `id` closed (expired, dismissed, or clicked).
    pub(crate) fn closed(&mut self, id: u32) {
        if let Some(key) = self.by_id.remove(&id)
            && self.by_key.get(&key) == Some(&id)
        {
            self.by_key.remove(&key);
        }
    }

    /// Forget `key` for a dismiss. Returns the id to close, if it is open.
    pub(crate) fn forget(&mut self, key: &NotifyKey) -> Option<u32> {
        let id = self.by_key.remove(key)?;
        self.by_id.remove(&id);
        Some(id)
    }

    pub(crate) fn key_of(&self, id: u32) -> Option<NotifyKey> {
        self.by_id.get(&id).cloned()
    }
}

pub(crate) struct Xdg {
    book: Arc<Mutex<IdBook>>,
    /// Server ids with a click waiter. A replaced notification keeps its id,
    /// so its waiter stays and no second one starts.
    waiting: Arc<Mutex<HashSet<u32>>>,
    clicks: ClickFn,
}

impl Xdg {
    pub(crate) fn new(clicks: ClickFn) -> Self {
        Self {
            book: Arc::new(Mutex::new(IdBook::default())),
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
            .hint(Hint::DesktopEntry(DESKTOP_ENTRY.into()))
            .urgency(Urgency::Normal)
            .action(OPEN_ACTION, "Open");
        let replaces = lock(&self.book).replace_id(&notification.key);
        if replaces != NEW_NOTIFICATION {
            message.id(replaces);
        }
        let handle = message.show().map_err(|_| BackendError("show"))?;
        let id = handle.id();
        lock(&self.book).shown(&notification.key, id);
        if !lock(&self.waiting).insert(id) {
            return Ok(());
        }
        let book = Arc::clone(&self.book);
        let waiting = Arc::clone(&self.waiting);
        let clicks = Arc::clone(&self.clicks);
        let spawned = thread::Builder::new()
            .name("thinwire-notify-click".into())
            .spawn(move || {
                // Returns on a click, or when the notification closes.
                handle.wait_for_action(|action| {
                    if action == OPEN_ACTION
                        && let Some(key) = lock(&book).key_of(id)
                    {
                        clicks(key);
                    }
                });
                // The id is gone on the server: never replace it again.
                lock(&book).closed(id);
                lock(&waiting).remove(&id);
            });
        if spawned.is_err() {
            lock(&self.waiting).remove(&id);
        }
        Ok(())
    }

    fn dismiss(&mut self, key: &NotifyKey) -> Result<(), BackendError> {
        let Some(id) = lock(&self.book).forget(key) else {
            return Ok(());
        };
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

#[cfg(test)]
mod tests {
    use super::*;
    use thinwire_core::ProtocolId;

    fn key(id: &str) -> NotifyKey {
        NotifyKey {
            protocol: ProtocolId::Telegram,
            conversation_id: id.into(),
        }
    }

    #[test]
    fn a_new_notification_replaces_nothing_and_a_replace_uses_our_own_id() {
        let mut book = IdBook::default();
        let ada = key("telegram:1");
        let bob = key("telegram:2");
        assert_eq!(book.replace_id(&ada), NEW_NOTIFICATION, "new: 0");

        book.shown(&ada, 582);
        assert_eq!(book.replace_id(&ada), 582, "replace: our own id");
        assert_eq!(book.replace_id(&bob), NEW_NOTIFICATION, "another chat: 0");
        assert_eq!(book.key_of(582), Some(ada.clone()));

        // The server let 582 expire (NotificationClosed): never reuse it.
        book.closed(582);
        assert_eq!(book.replace_id(&ada), NEW_NOTIFICATION, "after close: 0");
        assert_eq!(book.key_of(582), None);

        // The server can answer a replace with a new id: the old one leaves.
        book.shown(&ada, 600);
        book.shown(&ada, 601);
        assert_eq!(book.replace_id(&ada), 601);
        assert_eq!(book.key_of(600), None);
        book.closed(600);
        assert_eq!(
            book.replace_id(&ada),
            601,
            "a stale close keeps the open id"
        );

        assert_eq!(book.forget(&ada), Some(601));
        assert_eq!(book.replace_id(&ada), NEW_NOTIFICATION);
        assert_eq!(book.forget(&ada), None);
    }

    #[test]
    fn each_notification_names_the_app_and_a_normal_urgency() {
        let src = include_str!("xdg.rs");
        let show = &src[src.find("fn show(").expect("show")..];
        let show = &show[..show.find("fn dismiss(").expect("dismiss")];
        assert!(show.contains("Hint::DesktopEntry(DESKTOP_ENTRY.into())"));
        assert!(show.contains("Urgency::Normal"));
        assert!(
            show.contains("if replaces != NEW_NOTIFICATION"),
            "a new notification calls no .id()"
        );
        assert!(show.contains("lock(&book).closed(id)"));
    }
}
