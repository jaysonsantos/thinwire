//! Linux: freedesktop notifications over D-Bus.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;

use notify_rust::Urgency;

use crate::{Backend, BackendError, ClickFn, Notification, NotifyKey};

const APP_NAME: &str = "thinwire";
// No `desktop-entry` hint: the OS zips install no `thinwire.desktop` file,
// so the hint would name an entry that does not exist (qa on #87).
const OPEN_ACTION: &str = "default";

/// Most click-waiter threads at one time: one for each shown notification.
/// Over the limit, a notification shows with no click and no replace
/// (qa and #87 review: bound the waiter threads).
pub(crate) const MAX_CLICK_WAITERS: usize = 16;

/// A new click waiter may start while fewer than `MAX_CLICK_WAITERS` run.
pub(crate) const fn may_wait(running: usize) -> bool {
    running < MAX_CLICK_WAITERS
}
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
    /// Ids with no click waiter (over `MAX_CLICK_WAITERS`), per chat, at
    /// most `MAX_UNWATCHED_PER_CHAT`. A dismiss still closes them
    /// (#160 review). A replace never uses them: no waiter learns when they
    /// close.
    unwatched: HashMap<NotifyKey, Vec<u32>>,
    /// Unwatched ids in the order they came, for `MAX_UNWATCHED_TOTAL`. An
    /// entry whose id already left `unwatched` is skipped.
    unwatched_order: VecDeque<(NotifyKey, u32)>,
    /// Ids that a limit took out of the book while they can still show. The
    /// backend closes them after the command, so none stays on screen with
    /// no way to close it (#168 items 3 and 14).
    evicted: Vec<u32>,
}

/// Most unwatched ids kept for one chat. An older one expires by itself.
pub(crate) const MAX_UNWATCHED_PER_CHAT: usize = 8;

/// Most unwatched ids kept for all chats. The oldest goes first
/// (#160 review).
pub(crate) const MAX_UNWATCHED_TOTAL: usize = 64;

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
            // The server answered a replace with a new id. The old
            // notification can still show: keep it closable, never
            // replaceable (#168 item 2).
            self.shown_unwatched(key, old);
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
        // An old id kept after a replace leaves when its waiter sees it close.
        self.unwatched.retain(|_, ids| {
            ids.retain(|kept| *kept != id);
            !ids.is_empty()
        });
    }

    /// The server showed `key` with `id`, but no click waiter watches it.
    pub(crate) fn shown_unwatched(&mut self, key: &NotifyKey, id: u32) {
        let ids = self.unwatched.entry(key.clone()).or_default();
        // A known id is not added again: no second order entry (#168 item 1).
        if ids.contains(&id) {
            return;
        }
        ids.push(id);
        self.unwatched_order.push_back((key.clone(), id));
        if ids.len() > MAX_UNWATCHED_PER_CHAT {
            let oldest = ids.remove(0);
            self.evicted.push(oldest);
        }
        while self.unwatched.values().map(Vec::len).sum::<usize>() > MAX_UNWATCHED_TOTAL {
            let Some((owner, oldest)) = self.unwatched_order.pop_front() else {
                break;
            };
            if let Some(ids) = self.unwatched.get_mut(&owner)
                && let Some(at) = ids.iter().position(|kept| *kept == oldest)
            {
                ids.remove(at);
                if ids.is_empty() {
                    self.unwatched.remove(&owner);
                }
                self.evicted.push(oldest);
            }
        }
        // Drop entries of ids that already left, so the order list stays
        // small too.
        if self.unwatched_order.len() > 2 * MAX_UNWATCHED_TOTAL {
            let unwatched = &self.unwatched;
            self.unwatched_order
                .retain(|(owner, id)| unwatched.get(owner).is_some_and(|ids| ids.contains(id)));
        }
    }

    /// Forget `key` for a dismiss. The unwatched ids come first; the watched
    /// id is last.
    pub(crate) fn forget(&mut self, key: &NotifyKey) -> OpenIds {
        let watched = self.by_key.remove(key);
        if let Some(id) = watched {
            self.by_id.remove(&id);
        }
        OpenIds {
            unwatched: self.unwatched.remove(key).unwrap_or_default(),
            watched,
        }
    }

    /// A dismiss could not close these. A later dismiss still finds them.
    /// The watched id stays replaceable; an unwatched id does not.
    pub(crate) fn keep(&mut self, key: &NotifyKey, open: OpenIds) {
        if let Some(id) = open.watched {
            self.shown(key, id);
        }
        for id in open.unwatched {
            self.shown_unwatched(key, id);
        }
    }

    /// Ids that a limit took out of the book since the last call. The
    /// backend closes them.
    pub(crate) fn take_evicted(&mut self) -> Vec<u32> {
        std::mem::take(&mut self.evicted)
    }

    /// Take the unwatched ids of `key` out of the book.
    pub(crate) fn take_unwatched(&mut self, key: &NotifyKey) -> Vec<u32> {
        self.unwatched.remove(key).unwrap_or_default()
    }

    pub(crate) fn key_of(&self, id: u32) -> Option<NotifyKey> {
        self.by_id.get(&id).cloned()
    }
}

/// Ids taken out of the book for one dismiss.
#[derive(Debug, Default)]
pub(crate) struct OpenIds {
    unwatched: Vec<u32>,
    watched: Option<u32>,
}

impl OpenIds {
    #[cfg(test)]
    fn ids(&self) -> Vec<u32> {
        let mut ids = self.unwatched.clone();
        if let Some(id) = self.watched {
            ids.push(id);
        }
        ids
    }
}

/// Close every id of `key`. A failed close does not stop the rest. Ids that
/// stay open go back into the book (#160 review).
fn dismiss_ids(
    book: &Mutex<IdBook>,
    key: &NotifyKey,
    mut close: impl FnMut(u32) -> Result<(), BackendError>,
) -> Result<(), BackendError> {
    // The notifier thread runs one command at a time, so no `Show` of this
    // chat can land between `forget` and `keep` and be overwritten.
    let open = lock(book).forget(key);
    let mut failed = OpenIds::default();
    let mut first_error = None;
    for id in open.unwatched {
        if let Err(error) = close(id) {
            first_error.get_or_insert(error);
            failed.unwatched.push(id);
        }
    }
    if let Some(id) = open.watched
        && let Err(error) = close(id)
    {
        first_error.get_or_insert(error);
        failed.watched = Some(id);
    }
    if failed.watched.is_some() || !failed.unwatched.is_empty() {
        lock(book).keep(key, failed);
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// The OS calls that the id rules need. `Xdg` talks to D-Bus; the tests
/// use a fake (#168 item 15).
trait Server {
    fn close(&mut self, id: u32) -> Result<(), BackendError>;
    /// Show `notification` again, as a replace of its open id.
    fn replace(&mut self, notification: &Notification) -> Result<(), BackendError>;
}

/// An `Update`: close the chat's unwatched ids, then replace the watched
/// one while it is still open. A failed close does not stop the replace:
/// the watched notification must lose its old text too (#168 item 4). An
/// update never shows a new notification.
fn update_with(
    book: &Mutex<IdBook>,
    notification: &Notification,
    server: &mut impl Server,
) -> Result<(), BackendError> {
    let closed = close_unwatched(book, &notification.key, |id| server.close(id));
    if lock(book).replace_id(&notification.key) == NEW_NOTIFICATION {
        return closed;
    }
    let replaced = server.replace(notification);
    closed.and(replaced)
}

/// What `show` does with the id that the server returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Watch {
    /// A replace kept its id: its waiter goes on.
    Kept,
    /// Over `MAX_CLICK_WAITERS`: no waiter, the id is unwatched.
    Unwatched,
    /// Start a new click waiter for this id.
    Start,
}

/// Record the id that the server returned for `key`, and decide about its
/// click waiter.
fn after_show(
    book: &Mutex<IdBook>,
    waiting: &Mutex<HashSet<u32>>,
    key: &NotifyKey,
    id: u32,
) -> Watch {
    let mut waiters = lock(waiting);
    if waiters.contains(&id) {
        drop(waiters);
        lock(book).shown(key, id);
        return Watch::Kept;
    }
    if !may_wait(waiters.len()) {
        drop(waiters);
        // No waiter learns when this one closes: never replace it, but a
        // dismiss still closes it (#160 review).
        lock(book).shown_unwatched(key, id);
        return Watch::Unwatched;
    }
    waiters.insert(id);
    drop(waiters);
    lock(book).shown(key, id);
    Watch::Start
}

/// The freedesktop message for `notification`. `replaces` is the open id
/// of its chat, or `NEW_NOTIFICATION`: a new notification calls no `.id()`.
fn message_for(notification: &Notification, replaces: u32) -> notify_rust::Notification {
    let mut message = notify_rust::Notification::new();
    message
        .appname(APP_NAME)
        .summary(&notification.title)
        .body(&notification.body())
        .urgency(Urgency::Normal)
        .action(OPEN_ACTION, "Open");
    if replaces != NEW_NOTIFICATION {
        message.id(replaces);
    }
    message
}

/// Close the unwatched ids of `key` for an `Update`. They cannot be
/// replaced safely: no waiter knows if they expired. Closing them removes
/// the old text, for example after "Hide message text" (#160 qa). A failed
/// id goes back to the book for a later dismiss.
fn close_unwatched(
    book: &Mutex<IdBook>,
    key: &NotifyKey,
    mut close: impl FnMut(u32) -> Result<(), BackendError>,
) -> Result<(), BackendError> {
    let ids = lock(book).take_unwatched(key);
    let mut first_error = None;
    for id in ids {
        if let Err(error) = close(id) {
            first_error.get_or_insert(error);
            lock(book).shown_unwatched(key, id);
        }
    }
    first_error.map_or(Ok(()), Err)
}

/// No click thread watches `id`. It stays dismissible. A replace does not
/// use it, because nothing learns when it expires.
fn unwatch(book: &Mutex<IdBook>, waiting: &Mutex<HashSet<u32>>, key: &NotifyKey, id: u32) {
    lock(waiting).remove(&id);
    let mut book = lock(book);
    book.closed(id);
    book.shown_unwatched(key, id);
}

pub(crate) struct Xdg {
    book: Arc<Mutex<IdBook>>,
    /// Server ids with a click waiter. A replaced notification keeps its id,
    /// so its waiter stays and no second one starts.
    waiting: Arc<Mutex<HashSet<u32>>>,
    clicks: ClickFn,
    /// One session-bus connection for `CloseNotification`, made on first
    /// use and made again after an error (qa L2).
    bus: Option<zbus::blocking::Connection>,
}

impl Xdg {
    pub(crate) fn new(clicks: ClickFn) -> Self {
        Self {
            book: Arc::new(Mutex::new(IdBook::default())),
            waiting: Arc::new(Mutex::new(HashSet::new())),
            clicks,
            bus: None,
        }
    }

    /// Close the ids that a limit took out of the book. They are no longer
    /// tracked, so a failure only logs (#168 items 3 and 14).
    fn close_evicted(&mut self) {
        let evicted = lock(&self.book).take_evicted();
        for id in evicted {
            if self.close_notification(id).is_err() {
                tracing::debug!(kind = "evicted", "desktop notification close failed");
            }
        }
    }

    /// Show `notification`: a replace of its open id, or a new one.
    fn show_now(&mut self, notification: &Notification) -> Result<(), BackendError> {
        let replaces = lock(&self.book).replace_id(&notification.key);
        let handle = message_for(notification, replaces)
            .show()
            .map_err(|_| BackendError("show"))?;
        let id = handle.id();
        match after_show(&self.book, &self.waiting, &notification.key, id) {
            Watch::Kept => {}
            Watch::Unwatched => {
                tracing::debug!(kind = "no click waiter", "desktop notification shown");
            }
            Watch::Start => {
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
                                tracing::info!(kind = "open", "desktop notification clicked");
                                clicks(key);
                            }
                        });
                        // The id is gone on the server: never replace it again.
                        lock(&book).closed(id);
                        lock(&waiting).remove(&id);
                    });
                if spawned.is_err() {
                    // The id was recorded as watched before the thread
                    // started. Move it to the unwatched list so a replace
                    // does not reuse it and a dismiss can still close it.
                    unwatch(&self.book, &self.waiting, &notification.key, id);
                }
            }
        }
        Ok(())
    }

    /// `CloseNotification` by id. The click waiter owns the handle.
    fn close_notification(&mut self, id: u32) -> Result<(), BackendError> {
        let bus = match self.bus.take() {
            Some(bus) => bus,
            None => {
                zbus::blocking::Connection::session().map_err(|_| BackendError("session bus"))?
            }
        };
        let closed = bus.call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "CloseNotification",
            &(id,),
        );
        match closed {
            Ok(_) => {
                self.bus = Some(bus);
                Ok(())
            }
            // Drop the connection: the next dismiss makes a new one.
            Err(_) => Err(BackendError("close")),
        }
    }
}

impl Server for Xdg {
    fn close(&mut self, id: u32) -> Result<(), BackendError> {
        self.close_notification(id)
    }

    fn replace(&mut self, notification: &Notification) -> Result<(), BackendError> {
        self.show_now(notification)
    }
}

impl Backend for Xdg {
    fn show(&mut self, notification: &Notification) -> Result<(), BackendError> {
        let shown = self.show_now(notification);
        self.close_evicted();
        shown
    }

    /// Replace the notification only while its id is still open. A closed
    /// one stays closed: an update never shows a new notification.
    fn update(&mut self, notification: &Notification) -> Result<(), BackendError> {
        let book = Arc::clone(&self.book);
        let updated = update_with(&book, notification, self);
        self.close_evicted();
        updated
    }

    fn dismiss(&mut self, key: &NotifyKey) -> Result<(), BackendError> {
        // One close can fail. The others still close, and a failed id goes
        // back into the book so a later dismiss can find it (#160 review).
        let book = Arc::clone(&self.book);
        let dismissed = dismiss_ids(&book, key, |id| self.close_notification(id));
        self.close_evicted();
        dismissed
    }
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

        assert_eq!(book.forget(&ada).ids(), vec![601]);
        assert_eq!(book.replace_id(&ada), NEW_NOTIFICATION);
        assert!(book.forget(&ada).ids().is_empty());
    }

    #[test]
    fn each_notification_has_a_normal_urgency_and_no_desktop_entry() {
        use notify_rust::Hint;
        let message = message_for(&note("telegram:1"), NEW_NOTIFICATION);
        assert!(message.hints.contains(&Hint::Urgency(Urgency::Normal)));
        assert!(
            !message
                .hints
                .iter()
                .any(|hint| matches!(hint, Hint::DesktopEntry(_))),
            "no entry is installed"
        );
        assert!(message.actions.iter().any(|action| action == OPEN_ACTION));
    }

    #[test]
    fn a_notification_over_the_waiter_limit_can_still_be_dismissed() {
        let mut book = IdBook::default();
        let ada = key("telegram:1");
        book.shown(&ada, 10);
        // Over the limit: shown with no waiter.
        book.shown_unwatched(&ada, 11);
        assert_eq!(
            book.replace_id(&ada),
            10,
            "a replace uses only the watched id"
        );
        let bob = key("telegram:2");
        book.shown_unwatched(&bob, 20);
        assert_eq!(
            book.replace_id(&bob),
            NEW_NOTIFICATION,
            "never an unwatched id"
        );
        // A dismiss closes every id of the chat.
        let mut closing = book.forget(&ada).ids();
        closing.sort_unstable();
        assert_eq!(closing, vec![10, 11]);
        assert_eq!(book.forget(&bob).ids(), vec![20]);
        // The unwatched ids of one chat are bounded.
        for id in 0..(MAX_UNWATCHED_PER_CHAT as u32 + 5) {
            book.shown_unwatched(&bob, 100 + id);
        }
        assert_eq!(book.forget(&bob).ids().len(), MAX_UNWATCHED_PER_CHAT);
        // Bounded across chats too: the oldest goes first (#160 review).
        for n in 0..(MAX_UNWATCHED_TOTAL + 10) {
            book.shown_unwatched(&key(&format!("telegram:{n}")), 1000 + n as u32);
        }
        let total: usize = book.unwatched.values().map(Vec::len).sum();
        assert_eq!(total, MAX_UNWATCHED_TOTAL);
        assert!(
            book.forget(&key("telegram:0")).ids().is_empty(),
            "the oldest went"
        );
        assert_eq!(book.forget(&key("telegram:73")).ids(), vec![1073]);
        assert!(book.unwatched_order.len() <= 2 * MAX_UNWATCHED_TOTAL + 1);
    }

    #[test]
    fn an_update_closes_the_unwatched_ids_of_its_chat() {
        // More than `MAX_CLICK_WAITERS` shown: "Hide message text" must not
        // leave old text on screen in an unwatched notification (#160 qa).
        let book = Mutex::new(IdBook::default());
        let ada = key("telegram:1");
        let bob = key("telegram:2");
        {
            let mut book = lock(&book);
            book.shown(&ada, 10);
            book.shown_unwatched(&ada, 11);
            book.shown_unwatched(&ada, 12);
            book.shown_unwatched(&bob, 20);
        }
        let mut closed = Vec::new();
        let result = close_unwatched(&book, &ada, |id| {
            closed.push(id);
            if id == 12 {
                Err(BackendError("close"))
            } else {
                Ok(())
            }
        });
        assert_eq!(result, Err(BackendError("close")));
        assert_eq!(closed, vec![11, 12], "every unwatched id of the chat");
        let mut book = lock(&book);
        assert_eq!(
            book.replace_id(&ada),
            10,
            "the watched id is replaced, not closed"
        );
        let mut left = book.forget(&ada).ids();
        left.sort_unstable();
        assert_eq!(left, vec![10, 12], "the failed id stays for a dismiss");
        assert_eq!(
            book.forget(&bob).ids(),
            vec![20],
            "other chats are not touched"
        );
    }

    #[test]
    fn an_update_replaces_the_watched_id_even_after_a_failed_close() {
        let book = Mutex::new(IdBook::default());
        let ada = key("telegram:1");
        {
            let mut book = lock(&book);
            book.shown(&ada, 10);
            book.shown_unwatched(&ada, 11);
        }
        // The unwatched close fails: the watched notification still gets
        // the hidden text (#168 item 4).
        let mut server = FakeServer {
            fail_close: Some(11),
            ..FakeServer::default()
        };
        let result = update_with(&book, &note("telegram:1"), &mut server);
        assert_eq!(result, Err(BackendError("close")));
        assert_eq!(server.closed, vec![11]);
        assert_eq!(
            server.replaced,
            vec![ada.clone()],
            "replaced despite the failure"
        );

        // No watched id open: the unwatched ids close, nothing new shows.
        let book = Mutex::new(IdBook::default());
        lock(&book).shown_unwatched(&ada, 21);
        let mut server = FakeServer::default();
        assert_eq!(update_with(&book, &note("telegram:1"), &mut server), Ok(()));
        assert_eq!(server.closed, vec![21]);
        assert!(
            server.replaced.is_empty(),
            "an update never shows a new one"
        );
    }

    #[test]
    fn click_waiters_are_bounded() {
        assert!(may_wait(0));
        assert!(may_wait(MAX_CLICK_WAITERS - 1));
        assert!(!may_wait(MAX_CLICK_WAITERS));
        let book = Mutex::new(IdBook::default());
        let waiting = Mutex::new(HashSet::new());
        let ada = key("telegram:1");
        assert_eq!(after_show(&book, &waiting, &ada, 1), Watch::Start);
        assert_eq!(
            after_show(&book, &waiting, &ada, 1),
            Watch::Kept,
            "a replace keeps its waiter"
        );
        for id in 2..=MAX_CLICK_WAITERS as u32 {
            assert_eq!(
                after_show(&book, &waiting, &key(&format!("c:{id}")), id),
                Watch::Start
            );
        }
        let bob = key("telegram:2");
        assert_eq!(
            after_show(&book, &waiting, &bob, 99),
            Watch::Unwatched,
            "no new thread over the limit"
        );
        assert_eq!(lock(&waiting).len(), MAX_CLICK_WAITERS);
        assert_eq!(
            lock(&book).replace_id(&bob),
            NEW_NOTIFICATION,
            "never replaced"
        );
        assert_eq!(
            lock(&book).forget(&bob).ids(),
            vec![99],
            "still dismissible"
        );
    }

    #[test]
    fn a_failed_close_keeps_the_other_ids_dismissible() {
        let book = Mutex::new(IdBook::default());
        let ada = key("telegram:1");
        {
            let mut book = lock(&book);
            book.shown(&ada, 10);
            book.shown_unwatched(&ada, 11);
            book.shown_unwatched(&ada, 12);
        }
        let mut closed = Vec::new();
        let result = dismiss_ids(&book, &ada, |id| {
            if id == 11 {
                Err(BackendError("close"))
            } else {
                closed.push(id);
                Ok(())
            }
        });
        assert_eq!(result, Err(BackendError("close")));
        assert_eq!(closed, vec![12, 10], "every id is attempted");
        assert_eq!(
            lock(&book).replace_id(&ada),
            NEW_NOTIFICATION,
            "the failed id was unwatched"
        );
        assert_eq!(lock(&book).forget(&ada).ids(), vec![11]);

        // The watched id fails; the unwatched ones still close.
        {
            let mut book = lock(&book);
            book.shown(&ada, 20);
            book.shown_unwatched(&ada, 21);
            book.shown_unwatched(&ada, 22);
        }
        let mut closed = Vec::new();
        let result = dismiss_ids(&book, &ada, |id| {
            if id == 20 {
                Err(BackendError("close"))
            } else {
                closed.push(id);
                Ok(())
            }
        });
        assert_eq!(result, Err(BackendError("close")));
        closed.sort_unstable();
        assert_eq!(closed, vec![21, 22]);
        assert_eq!(lock(&book).replace_id(&ada), 20, "still replaceable");
        // A later dismiss finds the id the first close missed.
        let mut later = Vec::new();
        assert_eq!(
            dismiss_ids(&book, &ada, |id| {
                later.push(id);
                Ok(())
            }),
            Ok(())
        );
        assert_eq!(later, vec![20]);
        assert!(lock(&book).forget(&ada).ids().is_empty());
    }

    #[test]
    fn a_failed_click_thread_leaves_the_id_dismissible() {
        let book = Mutex::new(IdBook::default());
        let waiting = Mutex::new(HashSet::from([7]));
        let ada = key("telegram:1");
        lock(&book).shown(&ada, 7);
        unwatch(&book, &waiting, &ada, 7);
        assert!(lock(&waiting).is_empty());
        assert_eq!(lock(&book).replace_id(&ada), NEW_NOTIFICATION);
        assert_eq!(lock(&book).forget(&ada).ids(), vec![7]);
    }

    #[derive(Default)]
    struct FakeServer {
        closed: Vec<u32>,
        replaced: Vec<NotifyKey>,
        fail_close: Option<u32>,
    }

    impl Server for FakeServer {
        fn close(&mut self, id: u32) -> Result<(), BackendError> {
            self.closed.push(id);
            if self.fail_close == Some(id) {
                Err(BackendError("close"))
            } else {
                Ok(())
            }
        }

        fn replace(&mut self, notification: &Notification) -> Result<(), BackendError> {
            self.replaced.push(notification.key.clone());
            Ok(())
        }
    }

    fn note(id: &str) -> Notification {
        Notification {
            key: key(id),
            title: "Ada".into(),
            sender: None,
            preview: "hi".into(),
            count: 1,
        }
    }

    #[test]
    fn a_known_unwatched_id_adds_no_second_order_entry() {
        let mut book = IdBook::default();
        let ada = key("telegram:1");
        book.shown_unwatched(&ada, 5);
        book.shown_unwatched(&ada, 5);
        assert_eq!(book.unwatched_order.len(), 1, "#168 item 1");
        assert_eq!(book.forget(&ada).ids(), vec![5]);
    }

    #[test]
    fn a_replace_with_a_new_id_keeps_the_old_one_closable() {
        let mut book = IdBook::default();
        let ada = key("telegram:1");
        book.shown(&ada, 600);
        // The server answered the replace with a new id (#168 item 2).
        book.shown(&ada, 601);
        assert_eq!(book.replace_id(&ada), 601, "the new id is replaced next");
        let mut open = book.forget(&ada).ids();
        open.sort_unstable();
        assert_eq!(open, vec![600, 601], "a dismiss closes the old one too");

        // When the old one closes on the server, it leaves the book.
        book.shown(&ada, 700);
        book.shown(&ada, 701);
        book.closed(700);
        assert_eq!(book.forget(&ada).ids(), vec![701]);
    }

    #[test]
    fn an_id_that_a_limit_drops_is_handed_over_to_close() {
        let mut book = IdBook::default();
        let ada = key("telegram:1");
        // Per chat (#168 item 3).
        for id in 0..=MAX_UNWATCHED_PER_CHAT as u32 {
            book.shown_unwatched(&ada, id);
        }
        assert_eq!(book.take_evicted(), vec![0], "the oldest id of the chat");
        assert!(book.take_evicted().is_empty(), "handed over once");
        book.forget(&ada);
        // Across chats (#168 item 14).
        for n in 0..=MAX_UNWATCHED_TOTAL as u32 {
            book.shown_unwatched(&key(&format!("c:{n}")), 1000 + n);
        }
        assert_eq!(
            book.take_evicted(),
            vec![1000],
            "the oldest id of all chats"
        );
    }
}
