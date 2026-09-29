//! Desktop notification rules (#32).
//!
//! Pure logic: no OS call and no clock read here. The core decides, and a
//! frontend shows the result (egui through `thinwire-notify`; a TUI can
//! ignore it). Message text never goes to a log: `Notification` has a
//! manual `Debug` that prints only the chat key and the count.

use std::collections::{HashMap, VecDeque};
use std::fmt;

use thinwire_protocol::{Arrival, ChatMessage, Conversation, ProtocolId};

use crate::mutes::ChatMute;

/// A live message older than this does not notify. After a reconnect, a
/// protocol can push a burst of old messages; they count as unread only.
pub const STALE_AFTER_SECS: i64 = 300;

/// Most commands that wait for a frontend. A frontend that never reads them
/// does not grow memory.
pub const QUEUE_LIMIT: usize = 64;

/// Longest preview, in characters.
pub const PREVIEW_CHARS: usize = 120;

/// Text of a notification when "Hide message text" is on.
pub const HIDDEN_PREVIEW: &str = "New message";

/// One notification per chat.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NotifyKey {
    pub protocol: ProtocolId,
    pub conversation_id: String,
}

/// What a frontend shows. A new message in the same chat replaces it.
#[derive(Clone, PartialEq, Eq)]
pub struct Notification {
    pub key: NotifyKey,
    /// Chat title.
    pub title: String,
    /// Sender name in a group. `None` in a private chat, a channel, or when
    /// the preview is hidden.
    pub sender: Option<String>,
    /// Newest message text, at most `PREVIEW_CHARS`, or `HIDDEN_PREVIEW`.
    pub preview: String,
    /// New messages in this chat since the user looked at it.
    pub count: u32,
}

impl Notification {
    /// Body line: optional sender, the preview, and the count when more than
    /// one message waits.
    #[must_use]
    pub fn body(&self) -> String {
        let text = match &self.sender {
            Some(sender) => format!("{sender}: {}", self.preview),
            None => self.preview.clone(),
        };
        if self.count > 1 {
            format!("{text} ({} new)", self.count)
        } else {
            text
        }
    }
}

impl fmt::Debug for Notification {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Notification")
            .field("key", &self.key)
            .field("count", &self.count)
            .field("text", &"<redacted>")
            .finish()
    }
}

/// Output of the core for a frontend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotifyCommand {
    Show(Notification),
    /// Change a notification that already shows, for example to hide its
    /// text. It never shows a new one: a backend that cannot replace a
    /// shown notification ignores it, so no second notification with the
    /// old text appears (#160 review).
    Update(Notification),
    Dismiss(NotifyKey),
}

impl NotifyCommand {
    /// The chat this command is about.
    #[must_use]
    pub const fn key(&self) -> &NotifyKey {
        match self {
            Self::Show(notification) | Self::Update(notification) => &notification.key,
            Self::Dismiss(key) => key,
        }
    }

    /// `Show` or `Update`: it holds text and may be dropped when the queue
    /// is full. A `Dismiss` is never dropped.
    #[must_use]
    pub const fn is_content(&self) -> bool {
        !matches!(self, Self::Dismiss(_))
    }
}

/// Facts for one decision. The caller reads them from the state and the
/// settings.
pub(crate) struct NotifyContext<'a> {
    pub enabled: bool,
    pub preview: bool,
    pub window_focused: bool,
    pub viewed: Option<(ProtocolId, &'a str)>,
    pub has_session: bool,
    /// The chat of the message is muted in thinwire (#153).
    pub muted_here: bool,
    /// Unix seconds.
    pub now: i64,
}

/// Why a message does not notify. For tests only; never logged with data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SkipReason {
    Disabled,
    History,
    Outbound,
    NoSession,
    UnknownChat,
    Muted,
    Viewed,
    Stale,
}

/// The notification rules, in this order.
pub(crate) fn decide(
    message: &ChatMessage,
    chat: Option<&Conversation>,
    ctx: &NotifyContext<'_>,
) -> Result<(), SkipReason> {
    if !ctx.enabled {
        return Err(SkipReason::Disabled);
    }
    if message.arrival != Arrival::Live {
        return Err(SkipReason::History);
    }
    // Our own message, also one sent from another device of the account.
    if message.outbound {
        return Err(SkipReason::Outbound);
    }
    if !ctx.has_session {
        return Err(SkipReason::NoSession);
    }
    let Some(chat) = chat else {
        return Err(SkipReason::UnknownChat);
    };
    // A thinwire mute counts like a protocol mute (#153).
    if ChatMute::of(chat.muted, ctx.muted_here).is_muted() {
        return Err(SkipReason::Muted);
    }
    let seen = ctx.viewed == Some((message.protocol, message.conversation_id.as_str()));
    if ctx.window_focused && seen {
        return Err(SkipReason::Viewed);
    }
    if message.sent_at != 0 && ctx.now - message.sent_at > STALE_AFTER_SECS {
        return Err(SkipReason::Stale);
    }
    Ok(())
}

/// What the state says now about the chat of a pending notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChatNow {
    /// The chat left the list (removed, archived, or its session ended).
    Gone,
    /// The chat row and its unread count.
    Listed { unread: u32 },
}

/// One shown notification of a chat.
struct Pending {
    shown: Notification,
    /// The chat had unread messages after this notification. A later count
    /// of zero means the user read it on another device. A row with a stale
    /// zero can come before the new count, so zero alone is not enough.
    saw_unread: bool,
}

/// Pending notifications and the queue for the frontend.
pub(crate) struct Notifications {
    pending: HashMap<NotifyKey, Pending>,
    queue: VecDeque<NotifyCommand>,
    window_focused: bool,
    closing: bool,
}

impl Notifications {
    pub(crate) fn new() -> Self {
        Self {
            pending: HashMap::new(),
            queue: VecDeque::new(),
            // A new window has focus until the frontend says otherwise.
            window_focused: true,
            closing: false,
        }
    }

    pub(crate) const fn window_focused(&self) -> bool {
        self.window_focused
    }

    pub(crate) fn set_focus(&mut self, focused: bool) {
        self.window_focused = focused;
    }

    /// Queue a notification for `message` if the rules allow it.
    pub(crate) fn on_message(
        &mut self,
        message: &ChatMessage,
        chat: Option<&Conversation>,
        ctx: &NotifyContext<'_>,
    ) -> Result<(), SkipReason> {
        if self.closing {
            return Err(SkipReason::Disabled);
        }
        if let Err(reason) = decide(message, chat, ctx) {
            // For a live test: which rule skipped it. No chat id and no text.
            tracing::debug!(reason = ?reason, protocol = ?message.protocol, "no notification");
            return Err(reason);
        }
        let Some(chat) = chat else {
            return Err(SkipReason::UnknownChat);
        };
        let key = NotifyKey {
            protocol: message.protocol,
            conversation_id: message.conversation_id.clone(),
        };
        let count = self
            .pending
            .get(&key)
            .map_or(0, |pending| pending.shown.count)
            + 1;
        let notification = Notification {
            key: key.clone(),
            title: chat.title.clone(),
            sender: (ctx.preview && chat.is_group).then(|| message.sender.clone()),
            preview: if ctx.preview {
                message.body.chars().take(PREVIEW_CHARS).collect()
            } else {
                HIDDEN_PREVIEW.into()
            },
            count,
        };
        self.pending.insert(
            key,
            Pending {
                shown: notification.clone(),
                saw_unread: false,
            },
        );
        self.show(notification);
        // Debug, not info: the line shows when messages arrive (qa on #87).
        tracing::debug!(protocol = ?message.protocol, "notification queued");
        Ok(())
    }

    /// Queue `notification`. It replaces a queued `Show` or `Update` of the
    /// same chat.
    fn show(&mut self, notification: Notification) {
        let key = notification.key.clone();
        self.queue
            .retain(|command| !(command.is_content() && command.key() == &key));
        self.push(NotifyCommand::Show(notification));
    }

    /// The switch "Show notifications" went off: remove every queued and
    /// shown notification (#87 review).
    pub(crate) fn clear_all(&mut self) {
        for key in self.pending_keys() {
            self.dismiss(&key);
        }
    }

    /// "Hide message text" went on: no queued or shown notification keeps
    /// its text or sender. A shown one is replaced by a hidden one
    /// (#87 review).
    pub(crate) fn hide_previews(&mut self) {
        for key in self.pending_keys() {
            let Some(pending) = self.pending.get_mut(&key) else {
                continue;
            };
            if pending.shown.preview == HIDDEN_PREVIEW && pending.shown.sender.is_none() {
                continue;
            }
            pending.shown.preview = HIDDEN_PREVIEW.into();
            pending.shown.sender = None;
            let hidden = pending.shown.clone();
            // Not shown yet: the queued `Show` gets the hidden text. Shown:
            // an `Update`, which a backend with no replace ignores.
            if let Some(NotifyCommand::Show(queued)) = self
                .queue
                .iter_mut()
                .find(|command| matches!(command, NotifyCommand::Show(queued) if queued.key == key))
            {
                *queued = hidden;
                continue;
            }
            self.queue
                .retain(|command| !matches!(command, NotifyCommand::Update(old) if old.key == key));
            self.push(NotifyCommand::Update(hidden));
        }
    }

    fn pending_keys(&self) -> Vec<NotifyKey> {
        let mut keys: Vec<NotifyKey> = self.pending.keys().cloned().collect();
        keys.sort_by(|a, b| {
            (a.protocol.display_name(), &a.conversation_id)
                .cmp(&(b.protocol.display_name(), &b.conversation_id))
        });
        keys
    }

    /// Remove the notification of this chat, if one shows.
    pub(crate) fn dismiss(&mut self, key: &NotifyKey) {
        if self.pending.remove(key).is_none() {
            return;
        }
        self.queue
            .retain(|command| !(command.is_content() && command.key() == key));
        self.push(NotifyCommand::Dismiss(key.clone()));
    }

    /// Dismiss the notification of a chat when the user looks at it, when
    /// its protocol session ended, when the chat left the list, or when the
    /// chat was read on another device. Runs after each state change.
    pub(crate) fn sync(
        &mut self,
        viewed: Option<(ProtocolId, &str)>,
        has_session: impl Fn(ProtocolId) -> bool,
        chat_now: impl Fn(&NotifyKey) -> ChatNow,
    ) {
        let mut gone = Vec::new();
        for key in self.pending_keys() {
            let seen =
                self.window_focused && viewed == Some((key.protocol, key.conversation_id.as_str()));
            let read_elsewhere = match chat_now(&key) {
                ChatNow::Gone => true,
                ChatNow::Listed { unread: 0 } => self
                    .pending
                    .get(&key)
                    .is_some_and(|pending| pending.saw_unread),
                ChatNow::Listed { .. } => {
                    if let Some(pending) = self.pending.get_mut(&key) {
                        pending.saw_unread = true;
                    }
                    false
                }
            };
            if seen || read_elsewhere || !has_session(key.protocol) {
                gone.push(key);
            }
        }
        for key in gone {
            self.dismiss(&key);
        }
    }

    /// The app is closing: no new notification goes out, and every shown
    /// one gets a `Dismiss`, so none stays on screen after exit
    /// (#87 review).
    pub(crate) fn close(&mut self) {
        self.clear_all();
        self.closing = true;
    }

    pub(crate) fn take(&mut self) -> Vec<NotifyCommand> {
        self.queue.drain(..).collect()
    }

    /// Add `command`. Over `QUEUE_LIMIT`, drop the oldest `Show` or
    /// `Update`. Never drop a `Dismiss`: each one closes a shown
    /// notification, and there is at most one for each pending chat
    /// (#87 review).
    fn push(&mut self, command: NotifyCommand) {
        self.queue.push_back(command);
        while self.queue.len() > QUEUE_LIMIT {
            let Some(index) = self.queue.iter().position(NotifyCommand::is_content) else {
                break;
            };
            self.queue.remove(index);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thinwire_protocol::Delivery;

    const NOW: i64 = 1_800_000_000;

    fn chat(protocol: ProtocolId, id: &str) -> Conversation {
        Conversation {
            protocol,
            id: id.into(),
            title: "Ada".into(),
            participant: "Ada".into(),
            preview: String::new(),
            unread: 1,
            order: 1,
            last_at: NOW,
            is_group: false,
            writable: true,
            muted: false,
            placeholder: false,
        }
    }

    fn message(protocol: ProtocolId, chat: &str, body: &str) -> ChatMessage {
        ChatMessage {
            protocol,
            conversation_id: chat.into(),
            id: format!("{chat}:1"),
            sender: "Bob".into(),
            body: body.into(),
            outbound: false,
            delivery: Delivery::Sent,
            sent_at: NOW,
            arrival: Arrival::Live,
        }
    }

    fn ctx<'a>(viewed: Option<(ProtocolId, &'a str)>, focused: bool) -> NotifyContext<'a> {
        NotifyContext {
            enabled: true,
            preview: true,
            window_focused: focused,
            viewed,
            has_session: true,
            muted_here: false,
            now: NOW,
        }
    }

    /// #153: a thinwire mute counts like a protocol mute, in every
    /// protocol. Slack, Discord, and Signal rows never carry a protocol
    /// mute, so there the thinwire mute is the only one.
    #[test]
    fn a_thinwire_mute_counts_like_a_protocol_mute() {
        let here = NotifyContext {
            muted_here: true,
            ..ctx(None, false)
        };
        for protocol in ProtocolId::ALL {
            let row = chat(protocol, "chat:1");
            let live = message(protocol, "chat:1", "hi");
            let protocol_muted = Conversation {
                muted: true,
                ..row.clone()
            };
            assert_eq!(decide(&live, Some(&row), &ctx(None, false)), Ok(()));
            assert_eq!(
                decide(&live, Some(&row), &here),
                Err(SkipReason::Muted),
                "muted in thinwire only: {protocol}"
            );
            assert_eq!(
                decide(&live, Some(&protocol_muted), &ctx(None, false)),
                Err(SkipReason::Muted),
                "muted in the protocol only: {protocol}"
            );
            assert_eq!(
                decide(&live, Some(&protocol_muted), &here),
                Err(SkipReason::Muted),
                "both: {protocol}"
            );
        }
    }

    #[test]
    fn the_rules_decide_for_every_protocol() {
        for protocol in [ProtocolId::Telegram, ProtocolId::Slack, ProtocolId::Discord] {
            let row = chat(protocol, "c:1");
            let live = message(protocol, "c:1", "hi");
            let other = Some((protocol, "c:2"));
            let here = Some((protocol, "c:1"));

            assert_eq!(decide(&live, Some(&row), &ctx(other, true)), Ok(()));
            assert_eq!(
                decide(&live, Some(&row), &ctx(here, false)),
                Ok(()),
                "unfocused"
            );
            assert_eq!(
                decide(&live, Some(&row), &ctx(None, true)),
                Ok(()),
                "form over thread"
            );
            assert_eq!(
                decide(&live, Some(&row), &ctx(here, true)),
                Err(SkipReason::Viewed)
            );
            let disabled = NotifyContext {
                enabled: false,
                ..ctx(other, true)
            };
            assert_eq!(
                decide(&live, Some(&row), &disabled),
                Err(SkipReason::Disabled)
            );
            let history = ChatMessage {
                arrival: Arrival::History,
                ..live.clone()
            };
            assert_eq!(
                decide(&history, Some(&row), &ctx(other, true)),
                Err(SkipReason::History)
            );
            let own = ChatMessage {
                outbound: true,
                ..live.clone()
            };
            assert_eq!(
                decide(&own, Some(&row), &ctx(other, true)),
                Err(SkipReason::Outbound),
                "own message, also from another device"
            );
            let muted = Conversation {
                muted: true,
                ..row.clone()
            };
            assert_eq!(
                decide(&live, Some(&muted), &ctx(other, true)),
                Err(SkipReason::Muted)
            );
            assert_eq!(
                decide(&live, None, &ctx(other, true)),
                Err(SkipReason::UnknownChat)
            );
            let no_session = NotifyContext {
                has_session: false,
                ..ctx(other, true)
            };
            assert_eq!(
                decide(&live, Some(&row), &no_session),
                Err(SkipReason::NoSession)
            );
            let stale = ChatMessage {
                sent_at: NOW - STALE_AFTER_SECS - 1,
                ..live.clone()
            };
            assert_eq!(
                decide(&stale, Some(&row), &ctx(other, true)),
                Err(SkipReason::Stale)
            );
            let edge = ChatMessage {
                sent_at: NOW - STALE_AFTER_SECS,
                ..live.clone()
            };
            assert_eq!(decide(&edge, Some(&row), &ctx(other, true)), Ok(()));
            let unknown_time = ChatMessage { sent_at: 0, ..live };
            assert_eq!(decide(&unknown_time, Some(&row), &ctx(other, true)), Ok(()));
        }
    }

    #[test]
    fn one_notification_per_chat_with_a_count_and_dismiss_on_view() {
        let mut notes = Notifications::new();
        notes.set_focus(false);
        let row = chat(ProtocolId::Telegram, "telegram:1");
        let group = Conversation {
            id: "telegram:2".into(),
            title: "Team".into(),
            is_group: true,
            ..row.clone()
        };
        for body in ["one", "two", "three"] {
            notes
                .on_message(
                    &message(ProtocolId::Telegram, "telegram:1", body),
                    Some(&row),
                    &ctx(None, false),
                )
                .expect("notifies");
        }
        notes
            .on_message(
                &message(ProtocolId::Telegram, "telegram:2", "hi team"),
                Some(&group),
                &ctx(None, false),
            )
            .expect("notifies");
        let shown = notes.take();
        assert_eq!(shown.len(), 2, "one per chat: {shown:?}");
        let NotifyCommand::Show(first) = &shown[0] else {
            panic!("show")
        };
        assert_eq!(first.count, 3);
        assert_eq!(first.body(), "three (3 new)");
        assert_eq!(first.sender, None, "a private chat names no sender");
        let NotifyCommand::Show(team) = &shown[1] else {
            panic!("show")
        };
        assert_eq!(team.body(), "Bob: hi team");

        // Focus with the chat open dismisses it.
        notes.sync(
            Some((ProtocolId::Telegram, "telegram:1")),
            |_| true,
            |_| ChatNow::Listed { unread: 1 },
        );
        assert_eq!(notes.take(), vec![]);
        notes.set_focus(true);
        notes.sync(
            Some((ProtocolId::Telegram, "telegram:1")),
            |_| true,
            |_| ChatNow::Listed { unread: 1 },
        );
        let key = NotifyKey {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
        };
        assert_eq!(notes.take(), vec![NotifyCommand::Dismiss(key)]);
        // The session ends: its notifications go.
        notes.sync(
            None,
            |protocol| protocol != ProtocolId::Telegram,
            |_| ChatNow::Listed { unread: 1 },
        );
        assert_eq!(notes.take().len(), 1);
    }

    #[test]
    fn hidden_preview_limits_and_close() {
        let mut notes = Notifications::new();
        let group = Conversation {
            is_group: true,
            ..chat(ProtocolId::Slack, "slack:1")
        };
        let hidden = NotifyContext {
            preview: false,
            ..ctx(None, false)
        };
        notes
            .on_message(
                &message(ProtocolId::Slack, "slack:1", "secret"),
                Some(&group),
                &hidden,
            )
            .expect("notifies");
        let NotifyCommand::Show(shown) = &notes.take()[0] else {
            panic!("show")
        };
        assert_eq!(shown.preview, HIDDEN_PREVIEW);
        assert_eq!(shown.sender, None, "no sender name either");
        assert!(!format!("{shown:?}").contains("secret"));
        assert!(
            !format!("{shown:?}").contains("Ada"),
            "Debug hides the title"
        );

        let long = "x".repeat(PREVIEW_CHARS * 2);
        notes
            .on_message(
                &message(ProtocolId::Slack, "slack:1", &long),
                Some(&group),
                &ctx(None, false),
            )
            .expect("notifies");
        let NotifyCommand::Show(shown) = &notes.take()[0] else {
            panic!("show")
        };
        assert_eq!(shown.preview.chars().count(), PREVIEW_CHARS);

        for n in 0..(QUEUE_LIMIT * 2) {
            let id = format!("slack:{n}");
            let row = chat(ProtocolId::Slack, &id);
            notes
                .on_message(
                    &message(ProtocolId::Slack, &id, "hi"),
                    Some(&row),
                    &ctx(None, false),
                )
                .expect("notifies");
        }
        assert_eq!(notes.take().len(), QUEUE_LIMIT, "no unbounded queue");

        // Shutdown dismisses every shown notification, more than the queue
        // limit: a `Dismiss` is never dropped (#87 review).
        notes.close();
        let closing = notes.take();
        assert_eq!(closing.len(), QUEUE_LIMIT * 2);
        assert!(
            closing
                .iter()
                .all(|command| matches!(command, NotifyCommand::Dismiss(_)))
        );
        let row = chat(ProtocolId::Slack, "slack:1");
        assert!(
            notes
                .on_message(
                    &message(ProtocolId::Slack, "slack:1", "hi"),
                    Some(&row),
                    &ctx(None, false)
                )
                .is_err()
        );
        assert!(notes.take().is_empty(), "nothing after Shutdown");
    }

    fn show_three(notes: &mut Notifications) {
        for n in 1..=3 {
            let id = format!("telegram:{n}");
            let group = Conversation {
                is_group: true,
                ..chat(ProtocolId::Telegram, &id)
            };
            notes
                .on_message(
                    &message(ProtocolId::Telegram, &id, "secret text"),
                    Some(&group),
                    &ctx(None, false),
                )
                .expect("notifies");
        }
        assert_eq!(notes.take().len(), 3);
    }

    #[test]
    fn the_privacy_switches_apply_to_shown_and_queued_notifications() {
        let mut notes = Notifications::new();
        show_three(&mut notes);
        // One more is still queued when the user hides the text.
        let row = Conversation {
            is_group: true,
            ..chat(ProtocolId::Telegram, "telegram:1")
        };
        notes
            .on_message(
                &message(ProtocolId::Telegram, "telegram:1", "queued text"),
                Some(&row),
                &ctx(None, false),
            )
            .expect("notifies");
        notes.hide_previews();
        let hidden = notes.take();
        assert_eq!(hidden.len(), 3, "one command for each chat: {hidden:?}");
        // Chat 1 was still queued: its `Show` got the hidden text. Chats 2
        // and 3 already show: an `Update`, never a second `Show`.
        assert!(
            matches!(&hidden[0], NotifyCommand::Show(shown) if shown.key.conversation_id == "telegram:1")
        );
        assert!(
            hidden[1..]
                .iter()
                .all(|command| matches!(command, NotifyCommand::Update(_)))
        );
        for command in &hidden {
            let (NotifyCommand::Show(shown) | NotifyCommand::Update(shown)) = command else {
                panic!("content")
            };
            assert_eq!(shown.preview, HIDDEN_PREVIEW);
            assert_eq!(shown.sender, None);
            assert!(!shown.body().contains("text"));
        }
        notes.hide_previews();
        assert!(notes.take().is_empty(), "already hidden: nothing new");

        // Off: every notification goes, none stays queued.
        notes
            .on_message(
                &message(ProtocolId::Telegram, "telegram:2", "late"),
                Some(&chat(ProtocolId::Telegram, "telegram:2")),
                &ctx(None, false),
            )
            .expect("notifies");
        notes.clear_all();
        let cleared = notes.take();
        assert_eq!(cleared.len(), 3);
        assert!(
            cleared
                .iter()
                .all(|command| matches!(command, NotifyCommand::Dismiss(_)))
        );
    }

    #[test]
    fn slack_dedup_window_matches_notification_staleness() {
        assert_eq!(thinwire_protocol::DEDUP_FRESH_SECS, STALE_AFTER_SECS);
    }

    #[test]
    fn a_chat_that_leaves_or_is_read_elsewhere_loses_its_notification() {
        let mut notes = Notifications::new();
        notes.set_focus(false);
        show_three(&mut notes);
        let key = |n: u8| NotifyKey {
            protocol: ProtocolId::Telegram,
            conversation_id: format!("telegram:{n}"),
        };
        // Chat 2 still shows a stale zero: no dismiss yet.
        let now = |unread_2: u32, listed_3: bool| {
            move |k: &NotifyKey| match k.conversation_id.as_str() {
                "telegram:2" => ChatNow::Listed { unread: unread_2 },
                "telegram:3" if !listed_3 => ChatNow::Gone,
                _ => ChatNow::Listed { unread: 1 },
            }
        };
        notes.sync(None, |_| true, now(0, true));
        assert!(notes.take().is_empty(), "a stale zero before the count");
        // The count goes up, then back to zero: read on another device.
        notes.sync(None, |_| true, now(2, true));
        assert!(notes.take().is_empty());
        notes.sync(None, |_| true, now(0, true));
        assert_eq!(notes.take(), vec![NotifyCommand::Dismiss(key(2))]);
        // Chat 3 leaves the list.
        notes.sync(None, |_| true, now(0, false));
        assert_eq!(notes.take(), vec![NotifyCommand::Dismiss(key(3))]);
    }
}
