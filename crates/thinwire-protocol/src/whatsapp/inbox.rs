//! WhatsApp chat list and message store. No whatsapp-rust types and no UI calls.
//!
//! The live client (feature `whatsapp-web`) maps history-sync chunks and live
//! message batches into these records on the worker. Default builds test the
//! same rules with fakes.

use std::collections::{HashMap, HashSet};

use crate::adapter::{AdapterEvent, Arrival, ChatMessage, Conversation, Delivery, ProtocolId};

/// Prefix on every WhatsApp conversation id. The rest is the chat JID.
pub(super) const CONVERSATION_PREFIX: &str = "whatsapp:";

/// Messages kept per chat. Older ones drop off the front.
const MESSAGES_PER_CHAT: usize = 200;

/// Handled ids kept per chat. Past this, the older half goes, oldest first
/// (#158 review).
const HANDLED_PER_CHAT: usize = 1_000;

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
    /// The user saw this chat up to this message time (Unix seconds). A later
    /// history chunk carries the phone's older unread count, so a chat read
    /// here changes its count only by new messages (#98). A message older
    /// than this time does not count. It only moves forward; a full
    /// `handled` set also moves it (#158 review).
    read_at: Option<i64>,
    /// Inbound ids handled since the read point, with their time and arrival
    /// number: counted, or seen in the read second. A repeat does not count
    /// again, also after its row left the capped cache (Codex r4131089163,
    /// r4131622220). An id older than the read point goes: the read point
    /// refuses it already (Codex r4132341446, #158 review). At most
    /// [`HANDLED_PER_CHAT`] ids.
    handled: HashMap<String, (i64, u64)>,
    /// Arrival number of the newest id in `handled`.
    handled_seq: u64,
    /// Server ids that still carry the local send clock. `confirm_send`
    /// keeps that clock until a server or live copy arrives. The read point
    /// skips them (Codex r4132341439).
    optimistic: HashSet<String>,
    /// Newest activity time from history sync. A dropped send falls back to
    /// it (Codex r4131089177).
    history_at: i64,
}

impl ChatRecord {
    /// The user sees every message of this chat now.
    fn mark_read(&mut self) {
        self.unread = 0;
        let read_at = self.count_from(self.server_time());
        // Rows of the read second were seen; a later copy must not count. A
        // new id in that second still counts. An older id needs no entry:
        // the read point refuses it (Codex r4132341446, #158 review).
        let seen: Vec<(String, i64)> = self
            .messages
            .iter()
            .filter(|row| !row.from_me && row.timestamp >= read_at)
            .map(|row| (row.id.clone(), row.timestamp))
            .collect();
        for (id, timestamp) in seen {
            self.remember(&id, timestamp);
        }
    }

    /// Move the read point forward to `point` and drop the handled ids older
    /// than it. Returns the read point. It never moves back.
    fn count_from(&mut self, point: i64) -> i64 {
        let point = self.read_at.map_or(point, |old| old.max(point));
        self.read_at = Some(point);
        self.handled.retain(|_, (timestamp, _)| *timestamp >= point);
        point
    }

    /// Note one inbound id as handled. `false` if it was handled before.
    fn remember(&mut self, id: &str, timestamp: i64) -> bool {
        if self.handled.contains_key(id) {
            return false;
        }
        self.handled_seq += 1;
        self.handled
            .insert(id.to_string(), (timestamp, self.handled_seq));
        if self.handled.len() > HANDLED_PER_CHAT {
            self.forget_older_half();
        }
        true
    }

    /// Keep the newer half of `handled`, by time and then by arrival, so a
    /// chat with equal times keeps a bound too (#158 review). The read point
    /// moves to the oldest time kept: an older message does not count. Only
    /// a repeat of a dropped id of that same second can count again.
    fn forget_older_half(&mut self) {
        let mut order: Vec<(i64, u64)> = self.handled.values().copied().collect();
        order.sort_unstable();
        let keep = order[order.len() / 2];
        self.handled.retain(|_, entry| *entry >= keep);
        self.count_from(keep.0);
    }

    /// The newest server time of this chat: the newest row that is not a
    /// local `pending:` send and not a confirmed send still on the local
    /// clock, or the history time. A local send time can be ahead of the
    /// server's clock, and a later inbound message with an earlier server
    /// time must still count (#158 review, Codex r4132341439).
    fn server_time(&self) -> i64 {
        self.messages
            .iter()
            .filter(|row| !row.id.starts_with(PENDING_PREFIX) && !self.optimistic.contains(&row.id))
            .map(|row| row.timestamp)
            .max()
            .unwrap_or(0)
            .max(self.history_at)
    }

    /// Count one new inbound message of a chat read here, from history or
    /// live. `true` if it adds to unread: not stored, not handled since the
    /// read, and not older than the read point. Both paths use this one rule
    /// (#158 review), so `handled` keeps no id older than the read point.
    fn counts_after_read(&mut self, message: &WaMessage) -> bool {
        let Some(read_at) = self.read_at else {
            return false;
        };
        if message.from_me || message.timestamp < read_at {
            return false;
        }
        let stored = self.messages.iter().any(|row| row.id == message.id);
        !stored && self.remember(&message.id, message.timestamp)
    }

    /// The chat time from what is still here: the newest row, or the
    /// history time.
    fn restore_time(&mut self) {
        let newest = self.messages.iter().map(|row| row.timestamp).max();
        self.timestamp = newest.unwrap_or(0).max(self.history_at);
    }
}

/// In-memory WhatsApp inbox for one linked device.
#[derive(Debug, Default)]
pub(super) struct Inbox {
    chats: HashMap<String, ChatRecord>,
    /// Push names from history sync, keyed by JID.
    names: HashMap<String, String>,
    open: Option<String>,
    next_pending: u64,
    /// Chats sent by the chat-list pages so far (newest first).
    listed: usize,
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
            record.history_at = record.history_at.max(chat.timestamp);
            record.timestamp = record.timestamp.max(chat.timestamp);
            let mut added: u32 = 0;
            for message in chat.messages {
                remember_name(&mut self.names, &message);
                record.timestamp = record.timestamp.max(message.timestamp);
                // A history copy carries the server clock (Codex r4132341439).
                record.optimistic.remove(&message.id);
                if record.counts_after_read(&message) {
                    added = added.saturating_add(1);
                }
                insert_message(&mut record.messages, message);
            }
            // The user reads the open chat, so a later chunk keeps it read.
            // A chat read here changes its count only by new messages (#98):
            // the chunk's count is the phone's, from before the read. Else
            // keep live increments: a chunk can carry an older count.
            if self.open.as_deref() == Some(chat.jid.as_str()) {
                record.mark_read();
            } else if record.read_at.is_some() {
                record.unread = record.unread.saturating_add(added);
            } else {
                record.unread = record.unread.max(chat.unread);
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
            // A live copy carries the server clock (Codex r4132341439).
            record.optimistic.remove(&message.id);
            let known = record.messages.iter().any(|row| row.id == message.id);
            // A repeat of a stored id is a refresh. Only the first sight of a
            // pushed message is live, so a history replay cannot notify (#32).
            let arrival = if known {
                Arrival::History
            } else {
                Arrival::Live
            };
            // After a local read, an id counts once: a redelivery after its
            // row left the capped cache is not new (Codex r4131622220). Only
            // an id that can count goes into `handled`, and the history rule
            // applies (#158 review).
            let counts = !known
                && !message.from_me
                && !is_open
                && (record.read_at.is_none() || record.counts_after_read(&message));
            if counts {
                record.unread = record.unread.saturating_add(1);
            }
            record.timestamp = record.timestamp.max(message.timestamp);
            insert_message(&mut record.messages, message.clone());
            if is_open {
                record.mark_read();
            }
            events.push(AdapterEvent::MessageReceived {
                message: self.chat_message(&message, arrival),
            });
            if let Some(upsert) = self.upsert_event(&jid) {
                events.push(upsert);
            }
        }
        events
    }

    /// Newest chats first, at most [`CHAT_PAGE`].
    /// The first page of the chat list: newest first, at most [`CHAT_PAGE`].
    /// Starts the page cursor again.
    pub(super) fn chat_page(&mut self) -> Vec<AdapterEvent> {
        self.listed = 0;
        self.next_chat_page()
    }

    /// The next page after the chats already sent (Codex r4103855629). Empty
    /// after the last page. A chat that moved between pages can come twice;
    /// an upsert is idempotent. New activity sends its own upsert anyway.
    pub(super) fn next_chat_page(&mut self) -> Vec<AdapterEvent> {
        let mut jids: Vec<&String> = self.chats.keys().collect();
        jids.sort_by(|a, b| {
            let left = self.chats.get(*a).map_or(0, |row| row.timestamp);
            let right = self.chats.get(*b).map_or(0, |row| row.timestamp);
            right.cmp(&left).then_with(|| a.cmp(b))
        });
        let page: Vec<AdapterEvent> = jids
            .into_iter()
            .skip(self.listed)
            .take(CHAT_PAGE)
            .filter_map(|jid| self.upsert_event(jid))
            .collect();
        self.listed += page.len();
        page
    }

    /// Mark a chat open and read. Returns the upsert and the latest messages.
    pub(super) fn open_chat(&mut self, jid: &str) -> Option<Vec<AdapterEvent>> {
        let record = self.chats.get_mut(jid)?;
        record.mark_read();
        self.open = Some(jid.to_string());
        let mut events = Vec::new();
        if let Some(upsert) = self.upsert_event(jid) {
            events.push(upsert);
        }
        let record = self.chats.get(jid)?;
        let skip = record.messages.len().saturating_sub(OPEN_CHAT_MESSAGES);
        events.extend(record.messages.iter().skip(skip).map(|message| {
            AdapterEvent::MessageReceived {
                message: self.chat_message(message, Arrival::History),
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
                message: self.chat_message(message, Arrival::History),
            })
            .collect();
        (page, start > 0)
    }

    /// The chat the user looks at now (`None`: no WhatsApp chat). Messages
    /// in the viewed chat do not raise unread; a chat the user left counts
    /// again. Viewing a known chat marks it read and returns its upsert.
    pub(super) fn view(&mut self, jid: Option<&str>) -> Option<AdapterEvent> {
        // The chat the user leaves was read up to its newest message. Same
        // rule as an open chat, so its seen ids stay handled (#158 review).
        if let Some(left) = self.open.take()
            && Some(left.as_str()) != jid
            && let Some(record) = self.chats.get_mut(&left)
        {
            record.mark_read();
        }
        self.open = jid.map(str::to_string);
        let jid = jid?;
        let record = self.chats.get_mut(jid)?;
        let had_unread = record.unread > 0;
        record.mark_read();
        if had_unread {
            self.upsert_event(jid)
        } else {
            None
        }
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
                message: self.chat_message(&message, Arrival::History),
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
        // An echo may already be stored under the server id, with its clock.
        // Keep that row. Renaming the pending row onto the same id and keeping
        // the first copy drops the echo when the local clock is behind, and
        // the read point then treats the local clock as server time (Codex
        // r4132803796).
        let server_copy = record
            .messages
            .iter()
            .find(|row| row.id == server_id && row.id != pending)
            .cloned();
        let Some(pending_at) = record.messages.iter().position(|row| row.id == pending) else {
            return Vec::new();
        };
        record.failed.remove(pending);
        // An id whose row left the cache has no clock to skip (#158 review).
        record
            .optimistic
            .retain(|id| record.messages.iter().any(|row| row.id == *id));
        let message = if let Some(server) = server_copy {
            record.messages.remove(pending_at);
            server
        } else {
            let row = &mut record.messages[pending_at];
            row.id.clone_from(&server_id);
            let message = row.clone();
            // The row keeps the local clock until a server copy arrives.
            record.optimistic.insert(server_id);
            message
        };
        let mut events = vec![AdapterEvent::MessageReplaced {
            protocol: ProtocolId::WhatsApp,
            conversation_id: conversation_id(jid),
            old_id: pending.to_string(),
            message: self.chat_message(&message, Arrival::History),
        }];
        events.extend(self.upsert_event(jid));
        events
    }

    /// Remove the row of a new send that the server did not accept. The
    /// text stays in the compose field (`SendRejected`), so no failed row
    /// stays next to it (#98).
    pub(super) fn drop_send(&mut self, jid: &str, pending: &str) -> Vec<AdapterEvent> {
        let Some(record) = self.chats.get_mut(jid) else {
            return Vec::new();
        };
        let before = record.messages.len();
        record.messages.retain(|row| row.id != pending);
        record.failed.remove(pending);
        if record.messages.len() == before {
            return Vec::new();
        }
        // The send moved the chat time; the dropped row takes it back.
        record.restore_time();
        let mut events = vec![AdapterEvent::MessagesRemoved {
            protocol: ProtocolId::WhatsApp,
            conversation_id: conversation_id(jid),
            message_ids: vec![pending.to_string()],
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
                muted: false,
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

    fn chat_message(&self, message: &WaMessage, arrival: Arrival) -> ChatMessage {
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
            arrival,
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

    /// A pushed row can notify. A repeat, an opened history page, and a
    /// pending send stay `History` (#32).
    #[test]
    fn a_pushed_message_is_live_and_a_replay_is_history() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        let first = inbox.apply_messages(vec![message(chat, "live", "new", 100)]);
        assert_eq!(received(&first).arrival, Arrival::Live);
        assert!(!upserts(&first)[0].muted);
        let again = inbox.apply_messages(vec![message(chat, "live", "new", 100)]);
        assert_eq!(received(&again).arrival, Arrival::History);
        let opened = inbox.open_chat(chat).expect("known chat");
        assert!(opened.iter().all(|event| match event {
            AdapterEvent::MessageReceived { message } => message.arrival == Arrival::History,
            _ => true,
        }));
        let (_, pending, _) = inbox.begin_send(chat, "out", 101);
        assert_eq!(
            received(std::slice::from_ref(&pending)).arrival,
            Arrival::History
        );
    }

    fn received(events: &[AdapterEvent]) -> &ChatMessage {
        events
            .iter()
            .find_map(|event| match event {
                AdapterEvent::MessageReceived { message } => Some(message),
                _ => None,
            })
            .expect("message")
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

    /// #98 (Codex r4104217030): a chat read here and then left stays read
    /// when a later history chunk repeats the phone's older unread count.
    /// Only inbound messages newer than the read count.
    #[test]
    fn a_chat_read_here_stays_read_after_a_later_chunk() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        let chunk = |unread: u32, messages: Vec<WaMessage>| HistoryChat {
            jid: chat.into(),
            name: None,
            unread,
            timestamp: 10,
            messages,
        };
        inbox.apply_history(
            vec![chunk(3, vec![message(chat, "a", "one", 10)])],
            Vec::new(),
        );
        assert!(inbox.view(Some(chat)).is_some(), "viewing marks it read");
        inbox.view(None);

        // The phone still reports 3 unread: the chat stays read.
        let events = inbox.apply_history(vec![chunk(3, Vec::new())], Vec::new());
        assert_eq!(upserts(&events)[0].unread, 0);

        // A newer inbound message in a chunk counts; an own message does not.
        let mut own = message(chat, "c", "mine", 12);
        own.from_me = true;
        let events = inbox.apply_history(
            vec![chunk(5, vec![message(chat, "b", "new", 11), own])],
            Vec::new(),
        );
        assert_eq!(upserts(&events)[0].unread, 1);

        // A chat never read here still takes the phone's count.
        let other = "222@s.whatsapp.net";
        let events = inbox.apply_history(
            vec![HistoryChat {
                jid: other.into(),
                name: None,
                unread: 4,
                timestamp: 1,
                messages: Vec::new(),
            }],
            Vec::new(),
        );
        assert_eq!(upserts(&events)[0].unread, 4);
    }

    /// Codex r4131089163: a chat read here counts every new inbound message,
    /// also past the 200-row cache and in the read second, and a repeat of
    /// a chunk does not count twice.
    #[test]
    fn unread_after_a_read_counts_events_not_cached_rows() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        let chunk = |messages: Vec<WaMessage>| HistoryChat {
            jid: chat.into(),
            name: None,
            unread: 0,
            timestamp: 10,
            messages,
        };
        inbox.apply_history(
            vec![chunk(vec![message(chat, "seen", "one", 10)])],
            Vec::new(),
        );
        inbox.view(Some(chat));
        inbox.view(None);

        let flood: Vec<WaMessage> = (0..(MESSAGES_PER_CHAT + 50))
            .map(|n| {
                message(
                    chat,
                    &format!("n{n:04}"),
                    "new",
                    11 + i64::try_from(n).expect("small"),
                )
            })
            .collect();
        let events = inbox.apply_history(vec![chunk(flood.clone())], Vec::new());
        let total = u32::try_from(MESSAGES_PER_CHAT + 50).expect("small");
        assert_eq!(upserts(&events)[0].unread, total, "past the row cache");
        let events = inbox.apply_history(vec![chunk(flood)], Vec::new());
        assert_eq!(upserts(&events)[0].unread, total, "a repeat does not count");

        // The read second: a new message there counts; a seen one does not.
        let mut inbox = Inbox::default();
        inbox.apply_history(
            vec![chunk(vec![message(chat, "seen", "one", 10)])],
            Vec::new(),
        );
        inbox.view(Some(chat));
        inbox.view(None);
        let events = inbox.apply_history(
            vec![chunk(vec![
                message(chat, "seen", "one", 10),
                message(chat, "same-second", "two", 10),
            ])],
            Vec::new(),
        );
        assert_eq!(upserts(&events)[0].unread, 1);
    }

    /// Codex r4131622220: after a local read, a live redelivery of an id
    /// that left the capped cache does not count again.
    #[test]
    fn a_redelivered_live_message_counts_once_after_a_read() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        inbox.apply_messages(vec![message(chat, "seen", "one", 1)]);
        inbox.view(Some(chat));
        inbox.view(None);
        let live: Vec<WaMessage> = (0..(MESSAGES_PER_CHAT + 50))
            .map(|n| {
                message(
                    chat,
                    &format!("l{n:04}"),
                    "new",
                    2 + i64::try_from(n).expect("small"),
                )
            })
            .collect();
        let first = live[0].clone();
        inbox.apply_messages(live);
        let total = u32::try_from(MESSAGES_PER_CHAT + 50).expect("small");
        assert!(
            !inbox.chats[chat]
                .messages
                .iter()
                .any(|row| row.id == first.id),
            "the first live row left the cache"
        );
        let events = inbox.apply_messages(vec![first]);
        assert_eq!(
            upserts(&events)[0].unread,
            total,
            "a redelivery does not count"
        );
    }

    /// Codex r4132341446: a read keeps every seen inbound id, not only the
    /// ids of the read second. After newer rows evict an older read id, a
    /// live redelivery does not count. A new id in that second still does.
    #[test]
    fn a_read_id_evicted_from_the_cache_does_not_count_again() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        inbox.apply_messages(vec![
            message(chat, "old", "one", 1),
            message(chat, "newer", "two", 2),
        ]);
        inbox.view(Some(chat));
        let flood: Vec<WaMessage> = (0..MESSAGES_PER_CHAT)
            .map(|n| {
                message(
                    chat,
                    &format!("n{n:04}"),
                    "new",
                    3 + i64::try_from(n).expect("small"),
                )
            })
            .collect();
        inbox.apply_messages(flood);
        assert!(
            !inbox.chats[chat].messages.iter().any(|row| row.id == "old"),
            "the older row left the cache"
        );
        inbox.view(None);
        let events = inbox.apply_messages(vec![message(chat, "old", "one", 1)]);
        assert_eq!(upserts(&events)[0].unread, 0, "a redelivery does not count");
        let read_at = inbox.chats[chat].read_at.expect("read");
        let events = inbox.apply_messages(vec![message(chat, "fresh", "same second", read_at)]);
        assert_eq!(
            upserts(&events)[0].unread,
            1,
            "a new id in the read second counts"
        );
    }

    /// #158 review: a read drops handled ids older than the read point.
    /// 10,000 messages with a read after each keep the set tiny.
    #[test]
    fn many_reads_keep_handled_small() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        let mut largest = 0;
        for n in 0..10_000_i64 {
            inbox.apply_messages(vec![message(chat, &format!("m{n}"), "new", n + 1)]);
            inbox.view(Some(chat));
            inbox.view(None);
            largest = largest.max(inbox.chats[chat].handled.len());
        }
        assert!(largest <= 2, "a read drops older ids, got {largest}");
        assert_eq!(inbox.chats[chat].unread, 0);
    }

    /// #158 review: a chat read once and then ignored keeps `handled` below
    /// the cap. A dropped id of an older second does not count twice.
    #[test]
    fn a_busy_chat_after_one_read_keeps_handled_below_the_cap() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        inbox.apply_messages(vec![message(chat, "seen", "one", 1)]);
        inbox.view(Some(chat));
        inbox.view(None);
        let flood: u32 = 10_000;
        for n in 0..flood {
            inbox.apply_messages(vec![message(
                chat,
                &format!("n{n:05}"),
                "new",
                2 + i64::from(n),
            )]);
            assert!(inbox.chats[chat].handled.len() <= HANDLED_PER_CHAT);
        }
        assert_eq!(inbox.chats[chat].unread, flood);
        assert!(!inbox.chats[chat].handled.contains_key("n00000"));
        let events = inbox.apply_messages(vec![message(chat, "n00000", "new", 2)]);
        assert_eq!(
            upserts(&events)[0].unread,
            flood,
            "a dropped id does not count again"
        );
        let events =
            inbox.apply_messages(vec![message(chat, "fresh", "new", 2 + i64::from(flood))]);
        assert_eq!(upserts(&events)[0].unread, flood + 1);
    }

    /// #158 review: messages with one equal time also keep `handled` below
    /// the cap. The oldest arrivals go first.
    #[test]
    fn equal_times_keep_handled_below_the_cap() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        inbox.apply_messages(vec![message(chat, "seen", "one", 0)]);
        inbox.view(Some(chat));
        inbox.view(None);
        let flood: u32 = 10_000;
        for n in 0..flood {
            inbox.apply_messages(vec![message(chat, &format!("n{n:05}"), "new", 0)]);
        }
        let record = &inbox.chats[chat];
        assert!(record.handled.len() <= HANDLED_PER_CHAT);
        assert!(
            !record.handled.contains_key("n00000"),
            "the oldest went first"
        );
        assert!(record.handled.contains_key("n09999"));
        assert_eq!(record.unread, flood);
    }

    /// #158 review: an id that cannot count does not go into `handled`.
    #[test]
    fn only_ids_that_can_count_are_handled() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        inbox.apply_messages(vec![message(chat, "a", "one", 1)]);
        inbox.view(Some(chat));
        inbox.view(None);
        let mut mine = message(chat, "mine", "sent on the phone", 2);
        mine.from_me = true;
        inbox.apply_messages(vec![mine]);
        assert!(!inbox.chats[chat].handled.contains_key("mine"));
    }

    /// #158 review: after a read, a message older than the read point does
    /// not count, live or from history. One rule for both paths.
    #[test]
    fn a_message_older_than_the_read_point_counts_on_neither_path() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        inbox.apply_messages(vec![message(chat, "a", "one", 10)]);
        inbox.view(Some(chat));
        inbox.view(None);
        let events = inbox.apply_messages(vec![message(chat, "late", "live", 5)]);
        assert_eq!(upserts(&events)[0].unread, 0);
        let events = inbox.apply_history(
            vec![HistoryChat {
                jid: chat.into(),
                name: None,
                unread: 0,
                timestamp: 10,
                messages: vec![message(chat, "old", "history", 6)],
            }],
            Vec::new(),
        );
        assert_eq!(upserts(&events)[0].unread, 0);
        let events = inbox.apply_messages(vec![message(chat, "new", "live", 10)]);
        assert_eq!(
            upserts(&events)[0].unread,
            1,
            "the read second still counts"
        );
    }

    /// #158 review: a confirmed send with no echo leaves `optimistic` when
    /// its row leaves the cache.
    #[test]
    fn a_confirmed_send_without_an_echo_leaves_with_its_row() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        let (pending, _, _) = inbox.begin_send(chat, "first", 1);
        inbox.confirm_send(chat, &pending, "SRV1".into());
        let flood: Vec<WaMessage> = (0..MESSAGES_PER_CHAT)
            .map(|n| {
                message(
                    chat,
                    &format!("n{n:04}"),
                    "new",
                    2 + i64::try_from(n).expect("small"),
                )
            })
            .collect();
        inbox.apply_messages(flood);
        let (pending, _, _) = inbox.begin_send(chat, "second", 1_000);
        inbox.confirm_send(chat, &pending, "SRV2".into());
        let optimistic = &inbox.chats[chat].optimistic;
        assert!(!optimistic.contains("SRV1"), "its row left the cache");
        assert!(optimistic.contains("SRV2"));
    }

    /// #158 review: the read point is server time, not a local send time.
    /// A later inbound message with an earlier server time still counts.
    #[test]
    fn the_read_point_uses_server_time_not_a_local_send() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        inbox.apply_messages(vec![message(chat, "a", "one", 10)]);
        // A local send with a clock far ahead of the server's.
        let _ = inbox.begin_send(chat, "mine", 1_000);
        inbox.view(Some(chat));
        inbox.view(None);
        let events = inbox.apply_history(
            vec![HistoryChat {
                jid: chat.into(),
                name: None,
                unread: 0,
                timestamp: 20,
                messages: vec![message(chat, "b", "server twenty", 20)],
            }],
            Vec::new(),
        );
        assert_eq!(upserts(&events)[0].unread, 1);
    }

    /// Codex r4132341439: `confirm_send` drops the `pending:` prefix and keeps
    /// the local clock. The read point must not use that clock. An inbound
    /// message between the last server time and the local time still counts.
    #[test]
    fn a_confirmed_local_send_does_not_move_the_read_point() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        inbox.apply_messages(vec![message(chat, "a", "one", 10)]);
        let (pending, _, _) = inbox.begin_send(chat, "mine", 1_000);
        inbox.confirm_send(chat, &pending, "SRV1".into());
        let record = inbox.chats.get(chat).expect("chat");
        assert!(
            record.messages.iter().any(|row| row.id == "SRV1"),
            "the row is no longer pending"
        );
        assert!(record.messages.iter().all(|row| row.id != pending));
        inbox.view(Some(chat));
        inbox.view(None);
        let events = inbox.apply_history(
            vec![HistoryChat {
                jid: chat.into(),
                name: None,
                unread: 0,
                timestamp: 20,
                messages: vec![message(chat, "b", "server twenty", 20)],
            }],
            Vec::new(),
        );
        assert_eq!(upserts(&events)[0].unread, 1);

        // A server copy replaces the local clock. The read point can move
        // to that time, and a message before it does not count.
        let mut echo = message(chat, "SRV1", "mine", 30);
        echo.from_me = true;
        inbox.apply_messages(vec![echo]);
        inbox.view(Some(chat));
        inbox.view(None);
        let events = inbox.apply_history(
            vec![HistoryChat {
                jid: chat.into(),
                name: None,
                unread: 0,
                timestamp: 40,
                messages: vec![
                    message(chat, "c", "before the echo", 25),
                    message(chat, "d", "after the echo", 40),
                ],
            }],
            Vec::new(),
        );
        assert_eq!(upserts(&events)[0].unread, 1);
    }

    /// Codex r4132803796: an echo under the server id can arrive before
    /// confirm, with a clock ahead of the local send. The row that survives
    /// keeps that server clock, so opening the chat does not take the local
    /// clock as the read point. A later message between the two clocks does
    /// not count.
    #[test]
    fn an_echo_before_confirm_keeps_the_server_clock() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        inbox.apply_messages(vec![message(chat, "a", "one", 40)]);
        let local = 50;
        let server = 80;
        let (pending, _, _) = inbox.begin_send(chat, "mine", local);
        let mut echo = message(chat, "SRV1", "mine", server);
        echo.from_me = true;
        inbox.apply_messages(vec![echo]);
        let events = inbox.confirm_send(chat, &pending, "SRV1".into());
        assert!(events.iter().any(|event| matches!(
            event,
            AdapterEvent::MessageReplaced { old_id, message, .. }
                if old_id == &pending && message.id == "SRV1" && message.sent_at == server
        )));

        let record = inbox.chats.get(chat).expect("chat");
        let rows: Vec<_> = record
            .messages
            .iter()
            .filter(|row| row.id == "SRV1")
            .collect();
        assert_eq!(rows.len(), 1, "the echo and the pending row are one");
        assert_eq!(rows[0].timestamp, server, "the server clock survives");
        assert!(
            !record.optimistic.contains("SRV1"),
            "a server-stamped row is not a local clock"
        );
        assert!(record.messages.iter().all(|row| row.id != pending));

        inbox.view(Some(chat));
        inbox.view(None);
        assert_eq!(
            inbox.chats[chat].read_at,
            Some(server),
            "the read point is the server clock"
        );
        let events = inbox.apply_history(
            vec![HistoryChat {
                jid: chat.into(),
                name: None,
                unread: 0,
                timestamp: server,
                messages: vec![
                    message(chat, "between", "after the local clock", local + 20),
                    message(chat, "after", "after the server clock", server + 10),
                ],
            }],
            Vec::new(),
        );
        assert_eq!(
            upserts(&events)[0].unread,
            1,
            "only the message after the server clock counts"
        );
    }

    /// Codex r4131089177: a dropped send gives the chat its time back.
    #[test]
    fn a_dropped_send_restores_the_chat_time() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        inbox.apply_history(
            vec![HistoryChat {
                jid: chat.into(),
                name: None,
                unread: 0,
                timestamp: 5,
                messages: vec![message(chat, "a", "before", 5)],
            }],
            Vec::new(),
        );
        let (pending, _, _) = inbox.begin_send(chat, "failed", 100);
        let events = inbox.drop_send(chat, &pending);
        let row = upserts(&events)[0];
        assert_eq!(row.order, 5);
        assert_eq!(row.last_at, 5);
        assert_eq!(row.preview, "before");

        // With no message left, the history time stays.
        let other = "222@s.whatsapp.net";
        inbox.apply_history(
            vec![HistoryChat {
                jid: other.into(),
                name: None,
                unread: 0,
                timestamp: 7,
                messages: Vec::new(),
            }],
            Vec::new(),
        );
        let (pending, _, _) = inbox.begin_send(other, "failed", 100);
        let events = inbox.drop_send(other, &pending);
        assert_eq!(upserts(&events)[0].order, 7);
    }

    /// Messages that arrive in the open chat move the read point, so leaving
    /// the chat later keeps them read.
    #[test]
    fn messages_seen_in_the_open_chat_stay_read_after_leaving() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        inbox.apply_messages(vec![message(chat, "a", "one", 1)]);
        inbox.view(Some(chat));
        inbox.apply_messages(vec![message(chat, "b", "two", 2)]);
        inbox.view(Some("222@s.whatsapp.net"));
        let events = inbox.apply_history(
            vec![HistoryChat {
                jid: chat.into(),
                name: None,
                unread: 2,
                timestamp: 2,
                messages: Vec::new(),
            }],
            Vec::new(),
        );
        assert_eq!(upserts(&events)[0].unread, 0);
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

    /// Codex r4103855629: chats past the first page stay reachable.
    #[test]
    fn chat_list_pages_past_the_first_page() {
        let mut inbox = Inbox::default();
        let total = CHAT_PAGE * 2 + 50;
        let chats: Vec<HistoryChat> = (0..total)
            .map(|n| HistoryChat {
                jid: format!("{n}@s.whatsapp.net"),
                name: None,
                unread: 0,
                timestamp: i64::try_from(n).expect("small"),
                messages: Vec::new(),
            })
            .collect();
        inbox.apply_history(chats, Vec::new());
        let mut seen = std::collections::HashSet::new();
        let first = inbox.chat_page();
        assert_eq!(first.len(), CHAT_PAGE);
        assert_eq!(
            upserts(&first)[0].id,
            format!("whatsapp:{}@s.whatsapp.net", total - 1),
            "newest first"
        );
        seen.extend(upserts(&first).iter().map(|row| row.id.clone()));
        let second = inbox.next_chat_page();
        assert_eq!(second.len(), CHAT_PAGE);
        seen.extend(upserts(&second).iter().map(|row| row.id.clone()));
        let third = inbox.next_chat_page();
        assert_eq!(third.len(), 50);
        seen.extend(upserts(&third).iter().map(|row| row.id.clone()));
        assert_eq!(seen.len(), total, "every chat is listed once");
        assert!(inbox.next_chat_page().is_empty(), "after the last page");
        assert_eq!(inbox.chat_page().len(), CHAT_PAGE, "the first page again");
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
