//! Desktop notification rules (#32).
//!
//! Pure logic: no OS call and no clock read here. The core decides, and a
//! frontend shows the result (egui through `thinwire-notify`; a TUI can
//! ignore it). Message text never goes to a log: `Notification` has a
//! manual `Debug` that prints only the chat key and the count.

use std::collections::{HashMap, VecDeque};
use std::fmt;

use thinwire_protocol::{Arrival, ChatMessage, Conversation, ProtocolId};

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
    Dismiss(NotifyKey),
}

/// Facts for one decision. The caller reads them from the state and the
/// settings.
pub(crate) struct NotifyContext<'a> {
    pub enabled: bool,
    pub preview: bool,
    pub window_focused: bool,
    pub viewed: Option<(ProtocolId, &'a str)>,
    pub has_session: bool,
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
    if chat.muted {
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

/// Pending notifications and the queue for the frontend.
pub(crate) struct Notifications {
    pending: HashMap<NotifyKey, u32>,
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
        decide(message, chat, ctx)?;
        let Some(chat) = chat else {
            return Err(SkipReason::UnknownChat);
        };
        let key = NotifyKey {
            protocol: message.protocol,
            conversation_id: message.conversation_id.clone(),
        };
        let count = self.pending.entry(key.clone()).or_insert(0);
        *count += 1;
        let notification = Notification {
            key: key.clone(),
            title: chat.title.clone(),
            sender: (ctx.preview && chat.is_group).then(|| message.sender.clone()),
            preview: if ctx.preview {
                message.body.chars().take(PREVIEW_CHARS).collect()
            } else {
                HIDDEN_PREVIEW.into()
            },
            count: *count,
        };
        // The newest notification of a chat replaces the one still queued.
        self.queue
            .retain(|command| !matches!(command, NotifyCommand::Show(shown) if shown.key == key));
        self.push(NotifyCommand::Show(notification));
        Ok(())
    }

    /// Remove the notification of this chat, if one shows.
    pub(crate) fn dismiss(&mut self, key: &NotifyKey) {
        if self.pending.remove(key).is_none() {
            return;
        }
        self.queue
            .retain(|command| !matches!(command, NotifyCommand::Show(shown) if &shown.key == key));
        self.push(NotifyCommand::Dismiss(key.clone()));
    }

    /// Dismiss the chat the user looks at, and every chat of a protocol
    /// whose session ended. Runs after each state change.
    pub(crate) fn sync(
        &mut self,
        viewed: Option<(ProtocolId, &str)>,
        has_session: impl Fn(ProtocolId) -> bool,
    ) {
        let mut gone: Vec<NotifyKey> = self
            .pending
            .keys()
            .filter(|key| {
                let seen = self.window_focused
                    && viewed == Some((key.protocol, key.conversation_id.as_str()));
                seen || !has_session(key.protocol)
            })
            .cloned()
            .collect();
        gone.sort_by(|a, b| a.conversation_id.cmp(&b.conversation_id));
        for key in gone {
            self.dismiss(&key);
        }
    }

    /// The app is closing: nothing more goes out.
    pub(crate) fn close(&mut self) {
        self.closing = true;
        self.queue.clear();
    }

    pub(crate) fn take(&mut self) -> Vec<NotifyCommand> {
        self.queue.drain(..).collect()
    }

    fn push(&mut self, command: NotifyCommand) {
        self.queue.push_back(command);
        while self.queue.len() > QUEUE_LIMIT {
            // Drop the oldest `Show`. A `Dismiss` is small and must arrive.
            if let Some(index) = self
                .queue
                .iter()
                .position(|command| matches!(command, NotifyCommand::Show(_)))
            {
                self.queue.remove(index);
            } else {
                self.queue.pop_front();
            }
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
            now: NOW,
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
        notes.sync(Some((ProtocolId::Telegram, "telegram:1")), |_| true);
        assert_eq!(notes.take(), vec![]);
        notes.set_focus(true);
        notes.sync(Some((ProtocolId::Telegram, "telegram:1")), |_| true);
        let key = NotifyKey {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
        };
        assert_eq!(notes.take(), vec![NotifyCommand::Dismiss(key)]);
        // The session ends: its notifications go.
        notes.sync(None, |protocol| protocol != ProtocolId::Telegram);
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

        notes.close();
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
}
