//! WhatsApp chat list and message store. No whatsapp-rust types and no UI calls.
//!
//! The live client (feature `whatsapp-web`) maps history-sync chunks and live
//! message batches into these records on the worker. Default builds test the
//! same rules with fakes.

use std::collections::{HashMap, HashSet};

use crate::adapter::{AdapterEvent, ChatMessage, Conversation, Delivery, ProtocolId};

/// Prefix on every WhatsApp conversation id. The rest is the chat JID.
pub(super) const CONVERSATION_PREFIX: &str = "whatsapp:";

/// Messages kept per chat. Older ones drop off the front.
const MESSAGES_PER_CHAT: usize = 200;

/// Messages sent to the UI when a chat opens.
pub(super) const OPEN_CHAT_MESSAGES: usize = 50;

/// Chats sent to the UI on one chat-list load.
pub(super) const CHAT_PAGE: usize = 200;

const PREVIEW_CHARS: usize = 80;
const OUTBOUND_SENDER: &str = "You";
const GROUP_SERVER: &str = "@g.us";
const PENDING_PREFIX: &str = "pending:";
const STATUS_BROADCAST: &str = "status@broadcast";
const BROADCAST_SERVER: &str = "@broadcast";
const NEWSLETTER_SERVER: &str = "@newsletter";

/// One text-like message from history sync or live traffic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WaMessage {
    pub chat_jid: String,
    pub id: String,
    pub from_me: bool,
    /// Sender push name, when the server sent one.
    pub sender_name: Option<String>,
    /// Sender JID. In a DM this is the chat JID. Used when no name is known.
    pub sender_jid: Option<String>,
    /// Plain text. Media becomes a short label.
    pub body: String,
    /// Unix seconds.
    pub timestamp: i64,
}

/// One conversation from a history-sync chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HistoryChat {
    pub jid: String,
    pub name: Option<String>,
    pub unread: u32,
    /// Unix seconds of the last activity.
    pub timestamp: i64,
    pub messages: Vec<WaMessage>,
}

#[derive(Debug, Clone, Default)]
struct ChatRecord {
    name: Option<String>,
    unread: u32,
    timestamp: i64,
    /// Sorted by `(timestamp, id)`. Capped at [`MESSAGES_PER_CHAT`].
    messages: Vec<WaMessage>,
    /// Local `pending:` ids whose send failed. The user can resend them.
    failed: HashSet<String>,
}

/// In-memory WhatsApp inbox for one linked device.
#[derive(Debug, Default)]
pub(super) struct Inbox {
    chats: HashMap<String, ChatRecord>,
    /// Push names from history sync, keyed by JID.
    names: HashMap<String, String>,
    open: Option<String>,
    next_pending: u64,
}

impl Inbox {
    pub(super) fn clear(&mut self) -> Vec<AdapterEvent> {
        let removed = self
            .chats
            .keys()
            .map(|jid| AdapterEvent::ConversationRemoved {
                protocol: ProtocolId::WhatsApp,
                id: conversation_id(jid),
            })
            .collect();
        *self = Self::default();
        removed
    }

    /// Merge one history-sync chunk. Returns upserts for the chats it touched.
    pub(super) fn apply_history(
        &mut self,
        chats: Vec<HistoryChat>,
        push_names: Vec<(String, String)>,
    ) -> Vec<AdapterEvent> {
        for (jid, name) in push_names {
            let name = name.trim().to_string();
            if !name.is_empty() {
                self.names.insert(jid, name);
            }
        }
        let mut touched = Vec::new();
        for chat in chats {
            if !is_inbox_chat(&chat.jid) {
                continue;
            }
            let record = self.chats.entry(chat.jid.clone()).or_default();
            if let Some(name) = chat.name.map(|name| name.trim().to_string())
                && !name.is_empty()
            {
                record.name = Some(name);
            }
            // The user reads the open chat, so a later chunk keeps it read.
            let is_open = self.open.as_deref() == Some(chat.jid.as_str());
            // Keep live increments: a later chunk can carry an older count.
            record.unread = if is_open {
                0
            } else {
                record.unread.max(chat.unread)
            };
            record.timestamp = record.timestamp.max(chat.timestamp);
            for message in chat.messages {
                remember_name(&mut self.names, &message);
                record.timestamp = record.timestamp.max(message.timestamp);
                insert_message(&mut record.messages, message);
            }
            touched.push(chat.jid);
        }
        touched.sort();
        touched.dedup();
        touched
            .iter()
            .filter_map(|jid| self.upsert_event(jid))
            .collect()
    }

    /// Merge live messages. Every message goes to the UI; unread grows only
    /// for inbound messages outside the open chat.
    pub(super) fn apply_messages(&mut self, messages: Vec<WaMessage>) -> Vec<AdapterEvent> {
        let mut events = Vec::new();
        for message in messages {
            if !is_inbox_chat(&message.chat_jid) || message.id.is_empty() {
                continue;
            }
            remember_name(&mut self.names, &message);
            let jid = message.chat_jid.clone();
            let is_open = self.open.as_deref() == Some(jid.as_str());
            let record = self.chats.entry(jid.clone()).or_default();
            let known = record.messages.iter().any(|row| row.id == message.id);
            if !known && !message.from_me && !is_open {
                record.unread = record.unread.saturating_add(1);
            }
            record.timestamp = record.timestamp.max(message.timestamp);
            insert_message(&mut record.messages, message.clone());
            events.push(AdapterEvent::MessageReceived {
                message: self.chat_message(&message),
            });
            if let Some(upsert) = self.upsert_event(&jid) {
                events.push(upsert);
            }
        }
        events
    }

    /// Newest chats first, at most [`CHAT_PAGE`].
    pub(super) fn chat_page(&self) -> Vec<AdapterEvent> {
        let mut jids: Vec<&String> = self.chats.keys().collect();
        jids.sort_by(|a, b| {
            let left = self.chats.get(*a).map_or(0, |row| row.timestamp);
            let right = self.chats.get(*b).map_or(0, |row| row.timestamp);
            right.cmp(&left).then_with(|| a.cmp(b))
        });
        jids.into_iter()
            .take(CHAT_PAGE)
            .filter_map(|jid| self.upsert_event(jid))
            .collect()
    }

    /// Mark a chat open and read. Returns the upsert and the latest messages.
    pub(super) fn open_chat(&mut self, jid: &str) -> Option<Vec<AdapterEvent>> {
        let record = self.chats.get_mut(jid)?;
        record.unread = 0;
        self.open = Some(jid.to_string());
        let mut events = Vec::new();
        if let Some(upsert) = self.upsert_event(jid) {
            events.push(upsert);
        }
        let record = self.chats.get(jid)?;
        let skip = record.messages.len().saturating_sub(OPEN_CHAT_MESSAGES);
        events.extend(record.messages.iter().skip(skip).map(|message| {
            AdapterEvent::MessageReceived {
                message: self.chat_message(message),
            }
        }));
        Some(events)
    }

    /// Up to `limit` messages older than `before`, oldest first, and whether
    /// still older rows exist. An unknown chat or id gives no rows and `false`.
    pub(super) fn older_page(
        &self,
        jid: &str,
        before: &str,
        limit: usize,
    ) -> (Vec<AdapterEvent>, bool) {
        let Some(record) = self.chats.get(jid) else {
            return (Vec::new(), false);
        };
        let Some(end) = record.messages.iter().position(|row| row.id == before) else {
            return (Vec::new(), false);
        };
        let start = end.saturating_sub(limit);
        let page = record.messages[start..end]
            .iter()
            .map(|message| AdapterEvent::MessageReceived {
                message: self.chat_message(message),
            })
            .collect();
        (page, start > 0)
    }

    /// The chat the user looks at now (`None`: no WhatsApp chat). Messages
    /// in the viewed chat do not raise unread; a chat the user left counts
    /// again. Viewing a known chat marks it read and returns its upsert.
    pub(super) fn view(&mut self, jid: Option<&str>) -> Option<AdapterEvent> {
        self.open = jid.map(str::to_string);
        let jid = jid?;
        let record = self.chats.get_mut(jid)?;
        if record.unread == 0 {
            return None;
        }
        record.unread = 0;
        self.upsert_event(jid)
    }

    #[must_use]
    pub(super) fn knows_chat(&self, jid: &str) -> bool {
        self.chats.contains_key(jid)
    }

    /// Show an outgoing message at once under a local id. The send result
    /// replaces it with [`Self::confirm_send`] or marks it failed with
    /// [`Self::fail_send`].
    /// Returns the local id, the pending row, and the chat upsert. The shell
    /// takes the sidebar preview and order from the upsert.
    pub(super) fn begin_send(
        &mut self,
        jid: &str,
        body: &str,
        now: i64,
    ) -> (String, AdapterEvent, Option<AdapterEvent>) {
        self.next_pending += 1;
        let pending = format!("{PENDING_PREFIX}{}", self.next_pending);
        let message = WaMessage {
            chat_jid: jid.to_string(),
            id: pending.clone(),
            from_me: true,
            sender_name: None,
            sender_jid: None,
            body: body.to_string(),
            timestamp: now,
        };
        let record = self.chats.entry(jid.to_string()).or_default();
        record.timestamp = record.timestamp.max(now);
        insert_message(&mut record.messages, message.clone());
        (
            pending,
            AdapterEvent::MessageReceived {
                message: self.chat_message(&message),
            },
            self.upsert_event(jid),
        )
    }

    pub(super) fn confirm_send(
        &mut self,
        jid: &str,
        pending: &str,
        server_id: String,
    ) -> Vec<AdapterEvent> {
        let Some(record) = self.chats.get_mut(jid) else {
            return Vec::new();
        };
        let Some(row) = record.messages.iter_mut().find(|row| row.id == pending) else {
            return Vec::new();
        };
        row.id = server_id;
        let message = row.clone();
        record.failed.remove(pending);
        // A live echo of the same send may already be stored under the server id.
        let mut seen = false;
        record.messages.retain(|row| {
            if row.id != message.id {
                return true;
            }
            let keep = !seen;
            seen = true;
            keep
        });
        let mut events = vec![AdapterEvent::MessageReplaced {
            protocol: ProtocolId::WhatsApp,
            conversation_id: conversation_id(jid),
            old_id: pending.to_string(),
            message: self.chat_message(&message),
        }];
        events.extend(self.upsert_event(jid));
        events
    }

    /// Keep the row and mark it failed, so the user can resend it.
    pub(super) fn fail_send(&mut self, jid: &str, pending: &str) -> Vec<AdapterEvent> {
        let Some(record) = self.chats.get_mut(jid) else {
            return Vec::new();
        };
        if !record.messages.iter().any(|row| row.id == pending) {
            return Vec::new();
        }
        record.failed.insert(pending.to_string());
        vec![delivery_event(jid, pending, Delivery::Failed)]
    }

    /// A failed outgoing row goes back to pending. Returns its body and the
    /// delivery event, or `None` when the row is not a failed send.
    pub(super) fn retry_send(&mut self, jid: &str, id: &str) -> Option<(String, AdapterEvent)> {
        let record = self.chats.get_mut(jid)?;
        if !record.failed.remove(id) {
            return None;
        }
        let body = record
            .messages
            .iter()
            .find(|row| row.id == id)?
            .body
            .clone();
        Some((body, delivery_event(jid, id, Delivery::Pending)))
    }

    fn upsert_event(&self, jid: &str) -> Option<AdapterEvent> {
        let record = self.chats.get(jid)?;
        let title = self.title(jid, record);
        let preview = record
            .messages
            .last()
            .map(|message| preview(&message.body))
            .unwrap_or_default();
        Some(AdapterEvent::ConversationUpsert {
            conversation: Conversation {
                protocol: ProtocolId::WhatsApp,
                id: conversation_id(jid),
                participant: if is_group(jid) {
                    "Group".into()
                } else {
                    title.clone()
                },
                title,
                preview,
                unread: record.unread,
                order: record.timestamp,
                last_at: record.timestamp,
                is_group: is_group(jid),
                writable: true,
                placeholder: false,
            },
        })
    }

    fn title(&self, jid: &str, record: &ChatRecord) -> String {
        if let Some(name) = &record.name {
            return name.clone();
        }
        if let Some(name) = self.names.get(jid) {
            return name.clone();
        }
        fallback_label(jid)
    }

    fn chat_message(&self, message: &WaMessage) -> ChatMessage {
        let sender = if message.from_me {
            OUTBOUND_SENDER.to_string()
        } else if let Some(name) = message.sender_name.as_ref().filter(|name| !name.is_empty()) {
            name.clone()
        } else {
            let jid = message
                .sender_jid
                .as_deref()
                .unwrap_or(message.chat_jid.as_str());
            self.names
                .get(jid)
                .cloned()
                .unwrap_or_else(|| fallback_label(jid))
        };
        let failed = self
            .chats
            .get(&message.chat_jid)
            .is_some_and(|record| record.failed.contains(&message.id));
        let delivery = if failed {
            Delivery::Failed
        } else if message.id.starts_with(PENDING_PREFIX) {
            Delivery::Pending
        } else {
            Delivery::Sent
        };
        ChatMessage {
            protocol: ProtocolId::WhatsApp,
            conversation_id: conversation_id(&message.chat_jid),
            id: message.id.clone(),
            sender,
            body: message.body.clone(),
            outbound: message.from_me,
            delivery,
            sent_at: message.timestamp,
        }
    }
}

fn delivery_event(jid: &str, id: &str, delivery: Delivery) -> AdapterEvent {
    AdapterEvent::MessageDelivery {
        protocol: ProtocolId::WhatsApp,
        conversation_id: conversation_id(jid),
        message_id: id.to_string(),
        delivery,
    }
}

#[must_use]
pub(super) fn conversation_id(jid: &str) -> String {
    format!("{CONVERSATION_PREFIX}{jid}")
}

/// Chat JID from a conversation id. Placeholder and malformed ids return `None`.
#[must_use]
pub(super) fn parse_conversation_id(id: &str) -> Option<&str> {
    let jid = id.strip_prefix(CONVERSATION_PREFIX)?;
    let (user, server) = jid.split_once('@')?;
    if user.is_empty() || server.is_empty() || jid.chars().any(char::is_whitespace) {
        return None;
    }
    Some(jid)
}

/// Status stories, broadcast lists, and channels stay out of the inbox.
fn is_inbox_chat(jid: &str) -> bool {
    !jid.is_empty()
        && jid != STATUS_BROADCAST
        && !jid.ends_with(BROADCAST_SERVER)
        && !jid.ends_with(NEWSLETTER_SERVER)
}

fn is_group(jid: &str) -> bool {
    jid.ends_with(GROUP_SERVER)
}

fn fallback_label(jid: &str) -> String {
    if is_group(jid) {
        return "Unnamed group".into();
    }
    let user = jid.split('@').next().unwrap_or(jid);
    let user = user.split(':').next().unwrap_or(user);
    if !user.is_empty() && user.chars().all(|ch| ch.is_ascii_digit()) {
        format!("+{user}")
    } else {
        "Unknown contact".into()
    }
}

fn remember_name(names: &mut HashMap<String, String>, message: &WaMessage) {
    if message.from_me {
        return;
    }
    let (Some(jid), Some(name)) = (&message.sender_jid, &message.sender_name) else {
        return;
    };
    let name = name.trim();
    if !name.is_empty() {
        names.insert(jid.clone(), name.to_string());
    }
}

fn insert_message(messages: &mut Vec<WaMessage>, message: WaMessage) {
    if let Some(existing) = messages.iter_mut().find(|row| row.id == message.id) {
        *existing = message;
    } else {
        messages.push(message);
    }
    messages.sort_by(|a, b| a.timestamp.cmp(&b.timestamp).then_with(|| a.id.cmp(&b.id)));
    // Drop the oldest rows first, but never a local `pending:` row. A pending
    // or failed send must stay until its result or a resend resolves it.
    let mut excess = messages.len().saturating_sub(MESSAGES_PER_CHAT);
    if excess > 0 {
        messages.retain(|row| {
            if excess == 0 || row.id.starts_with(PENDING_PREFIX) {
                return true;
            }
            excess -= 1;
            false
        });
    }
}

fn preview(body: &str) -> String {
    let line = body.lines().next().unwrap_or_default();
    let mut out: String = line.chars().take(PREVIEW_CHARS).collect();
    if line.chars().count() > PREVIEW_CHARS {
        out.push('…');
    }
    out
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub(in crate::whatsapp) fn message(chat: &str, id: &str, body: &str, ts: i64) -> WaMessage {
        WaMessage {
            chat_jid: chat.into(),
            id: id.into(),
            from_me: false,
            sender_name: Some("Ana".into()),
            sender_jid: Some(chat.into()),
            body: body.into(),
            timestamp: ts,
        }
    }

    fn upserts(events: &[AdapterEvent]) -> Vec<&Conversation> {
        events
            .iter()
            .filter_map(|event| match event {
                AdapterEvent::ConversationUpsert { conversation } => Some(conversation),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn history_builds_titles_previews_and_order() {
        let mut inbox = Inbox::default();
        let events = inbox.apply_history(
            vec![
                HistoryChat {
                    jid: "111@s.whatsapp.net".into(),
                    name: None,
                    unread: 2,
                    timestamp: 10,
                    messages: vec![message("111@s.whatsapp.net", "a", "hi\nsecond line", 10)],
                },
                HistoryChat {
                    jid: "g1@g.us".into(),
                    name: Some("Family".into()),
                    unread: 0,
                    timestamp: 20,
                    messages: Vec::new(),
                },
                HistoryChat {
                    jid: "222@s.whatsapp.net".into(),
                    name: None,
                    unread: 0,
                    timestamp: 5,
                    messages: Vec::new(),
                },
            ],
            vec![("222@s.whatsapp.net".into(), "Bo".into())],
        );
        let rows = upserts(&events);
        assert_eq!(rows.len(), 3);
        let by_id = |id: &str| *rows.iter().find(|row| row.id == id).expect("row");
        let ana = by_id("whatsapp:111@s.whatsapp.net");
        assert_eq!(ana.title, "Ana");
        assert_eq!(ana.preview, "hi");
        assert_eq!(ana.unread, 2);
        assert_eq!(ana.order, 10);
        let family = by_id("whatsapp:g1@g.us");
        assert_eq!(family.title, "Family");
        assert_eq!(family.participant, "Group");
        assert_eq!(by_id("whatsapp:222@s.whatsapp.net").title, "Bo");

        let page = inbox.chat_page();
        let order: Vec<&str> = upserts(&page).iter().map(|row| row.id.as_str()).collect();
        assert_eq!(
            order,
            vec![
                "whatsapp:g1@g.us",
                "whatsapp:111@s.whatsapp.net",
                "whatsapp:222@s.whatsapp.net",
            ]
        );
    }

    #[test]
    fn unknown_dm_falls_back_to_the_number() {
        let mut inbox = Inbox::default();
        let mut row = message("333@s.whatsapp.net", "x", "yo", 1);
        row.sender_name = None;
        let events = inbox.apply_messages(vec![row]);
        assert_eq!(upserts(&events)[0].title, "+333");
        let lid = inbox.apply_messages(vec![WaMessage {
            sender_name: None,
            ..message("abc@lid", "y", "yo", 1)
        }]);
        assert_eq!(upserts(&lid)[0].title, "Unknown contact");
    }

    #[test]
    fn open_chat_resets_unread_and_returns_latest_messages() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        let history: Vec<WaMessage> = (0..(OPEN_CHAT_MESSAGES as i64 + 5))
            .map(|n| message(chat, &format!("m{n:03}"), &format!("body {n}"), n))
            .collect();
        inbox.apply_history(
            vec![HistoryChat {
                jid: chat.into(),
                name: None,
                unread: 4,
                timestamp: 0,
                messages: history,
            }],
            Vec::new(),
        );
        let events = inbox.open_chat(chat).expect("known chat");
        assert_eq!(upserts(&events)[0].unread, 0);
        let bodies: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                AdapterEvent::MessageReceived { message } => Some(message.id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(bodies.len(), OPEN_CHAT_MESSAGES);
        assert_eq!(bodies.first().copied(), Some("m005"));
        assert_eq!(bodies.last().copied(), Some("m054"));
        assert!(inbox.open_chat("nope@s.whatsapp.net").is_none());

        // Inbound traffic in the open chat does not raise unread.
        let live = inbox.apply_messages(vec![message(chat, "live", "new", 100)]);
        assert_eq!(upserts(&live)[0].unread, 0);
        let other = inbox.apply_messages(vec![message("9@s.whatsapp.net", "o", "hey", 1)]);
        assert_eq!(upserts(&other)[0].unread, 1);
    }

    #[test]
    fn duplicate_live_messages_do_not_double_count() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        inbox.apply_messages(vec![message(chat, "a", "one", 1)]);
        let events = inbox.apply_messages(vec![message(chat, "a", "one", 1)]);
        assert_eq!(upserts(&events)[0].unread, 1);
    }

    #[test]
    fn send_pending_is_replaced_or_removed() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        let (pending, shown, upsert) = inbox.begin_send(chat, "hello", 50);
        match upsert {
            Some(AdapterEvent::ConversationUpsert { conversation }) => {
                assert_eq!(conversation.preview, "hello");
                assert_eq!(conversation.order, 50);
            }
            other => panic!("the send must update the chat row: {other:?}"),
        }
        match shown {
            AdapterEvent::MessageReceived { message } => {
                assert!(message.outbound);
                assert_eq!(message.sender, "You");
                assert_eq!(message.id, pending);
            }
            other => panic!("unexpected {other:?}"),
        }
        let events = inbox.confirm_send(chat, &pending, "SRV1".into());
        assert!(events.iter().any(|event| matches!(
            event,
            AdapterEvent::MessageReplaced { old_id, message, .. }
                if old_id == &pending && message.id == "SRV1"
        )));

        let (failed, _, _) = inbox.begin_send(chat, "again", 60);
        let events = inbox.fail_send(chat, &failed);
        assert_eq!(
            events,
            vec![delivery_event(chat, &failed, Delivery::Failed)]
        );
        let open = inbox.open_chat(chat).expect("chat");
        let rows: Vec<(String, Delivery)> = open
            .iter()
            .filter_map(|event| match event {
                AdapterEvent::MessageReceived { message } => {
                    Some((message.id.clone(), message.delivery))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                ("SRV1".to_string(), Delivery::Sent),
                (failed.clone(), Delivery::Failed)
            ]
        );

        let (body, event) = inbox.retry_send(chat, &failed).expect("failed row");
        assert_eq!(body, "again");
        assert_eq!(event, delivery_event(chat, &failed, Delivery::Pending));
        assert!(
            inbox.retry_send(chat, &failed).is_none(),
            "only a failed row"
        );
        assert!(inbox.retry_send(chat, "SRV1").is_none());
    }

    #[test]
    fn confirm_after_echo_keeps_one_copy() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        let (pending, _, _) = inbox.begin_send(chat, "hello", 50);
        let mut echo = message(chat, "SRV1", "hello", 50);
        echo.from_me = true;
        inbox.apply_messages(vec![echo]);
        inbox.confirm_send(chat, &pending, "SRV1".into());
        let open = inbox.open_chat(chat).expect("chat");
        let count = open
            .iter()
            .filter(|event| matches!(event, AdapterEvent::MessageReceived { .. }))
            .count();
        assert_eq!(count, 1);
    }

    /// Codex r4093058047: a chat the user left counts unread again.
    #[test]
    fn leaving_a_chat_counts_its_messages_unread_again() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        inbox.apply_messages(vec![message(chat, "a", "one", 1)]);
        let read = inbox.view(Some(chat)).expect("marks the chat read");
        assert!(matches!(
            read,
            AdapterEvent::ConversationUpsert { conversation } if conversation.unread == 0
        ));
        let events = inbox.apply_messages(vec![message(chat, "b", "two", 2)]);
        assert_eq!(upserts(&events)[0].unread, 0, "the viewed chat stays read");

        assert!(inbox.view(None).is_none());
        let events = inbox.apply_messages(vec![message(chat, "c", "three", 3)]);
        assert_eq!(upserts(&events)[0].unread, 1, "a left chat counts again");

        // Viewing another chat also leaves this one.
        inbox.view(Some("222@s.whatsapp.net"));
        let events = inbox.apply_messages(vec![message(chat, "d", "four", 4)]);
        assert_eq!(upserts(&events)[0].unread, 2);
    }

    /// Codex r4103183211: a later history chunk keeps live unread increments.
    #[test]
    fn later_history_chunk_keeps_live_unread() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        let chunk = |unread: u32| HistoryChat {
            jid: chat.into(),
            name: None,
            unread,
            timestamp: 1,
            messages: Vec::new(),
        };
        inbox.apply_history(vec![chunk(0)], Vec::new());
        inbox.apply_messages(vec![message(chat, "live", "hi", 5)]);
        let events = inbox.apply_history(vec![chunk(0)], Vec::new());
        assert_eq!(
            upserts(&events)[0].unread,
            1,
            "the live message stays unread"
        );
        let events = inbox.apply_history(vec![chunk(4)], Vec::new());
        assert_eq!(upserts(&events)[0].unread, 4, "a higher server count wins");
    }

    /// Codex r4093606402: a later history chunk keeps the open chat read.
    #[test]
    fn later_history_chunk_keeps_the_open_chat_read() {
        let mut inbox = Inbox::default();
        let chunk = |jid: &str, unread: u32| HistoryChat {
            jid: jid.into(),
            name: None,
            unread,
            timestamp: 1,
            messages: Vec::new(),
        };
        let open = "111@s.whatsapp.net";
        let other = "222@s.whatsapp.net";
        inbox.apply_history(vec![chunk(open, 3), chunk(other, 2)], Vec::new());
        inbox.open_chat(open).expect("chat");
        let events = inbox.apply_history(vec![chunk(open, 5), chunk(other, 4)], Vec::new());
        let unread = |id: &str| {
            upserts(&events)
                .into_iter()
                .find(|row| row.id == conversation_id(id))
                .expect("row")
                .unread
        };
        assert_eq!(unread(open), 0, "the open chat stays read");
        assert_eq!(unread(other), 4, "other chats take the server count");
    }

    /// Codex r4093471553: a busy chat must not evict an unresolved send.
    #[test]
    fn history_trim_keeps_pending_and_failed_sends() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        let (pending, _, _) = inbox.begin_send(chat, "in flight", 1);
        let (failed, _, _) = inbox.begin_send(chat, "failed", 2);
        inbox.fail_send(chat, &failed);
        let flood: Vec<WaMessage> = (0..(MESSAGES_PER_CHAT as i64 + 50))
            .map(|n| message(chat, &format!("m{n:04}"), "busy", 10 + n))
            .collect();
        inbox.apply_messages(flood);

        let events = inbox.confirm_send(chat, &pending, "SRV1".into());
        assert!(events.iter().any(|event| matches!(
            event,
            AdapterEvent::MessageReplaced { old_id, .. } if old_id == &pending
        )));
        assert!(
            inbox.retry_send(chat, &failed).is_some(),
            "failed row is retryable"
        );
        let record = inbox.chats.get(chat).expect("chat");
        assert!(record.messages.len() <= MESSAGES_PER_CHAT + 2);
        assert!(
            !record.messages.iter().any(|row| row.id == "m0000"),
            "the oldest history row is evicted"
        );
    }

    #[test]
    fn conversation_ids_round_trip_and_reject_placeholders() {
        assert_eq!(
            parse_conversation_id(&conversation_id("111@s.whatsapp.net")),
            Some("111@s.whatsapp.net")
        );
        assert_eq!(parse_conversation_id("whatsapp:placeholder"), None);
        assert_eq!(parse_conversation_id("telegram:1"), None);
        assert_eq!(parse_conversation_id("whatsapp:@g.us"), None);
        assert_eq!(parse_conversation_id("whatsapp:1 2@g.us"), None);
    }

    #[test]
    fn status_broadcast_and_channels_stay_out() {
        let mut inbox = Inbox::default();
        let events = inbox.apply_messages(vec![
            message("status@broadcast", "s", "story", 1),
            message("123@newsletter", "n", "channel", 1),
            message("1@broadcast", "b", "list", 1),
        ]);
        assert!(events.is_empty());
        assert!(inbox.chat_page().is_empty());
    }

    #[test]
    fn clear_removes_every_chat() {
        let mut inbox = Inbox::default();
        inbox.apply_messages(vec![message("1@s.whatsapp.net", "a", "x", 1)]);
        let events = inbox.clear();
        assert_eq!(
            events,
            vec![AdapterEvent::ConversationRemoved {
                protocol: ProtocolId::WhatsApp,
                id: "whatsapp:1@s.whatsapp.net".into(),
            }]
        );
        assert!(inbox.chat_page().is_empty());
    }
}
