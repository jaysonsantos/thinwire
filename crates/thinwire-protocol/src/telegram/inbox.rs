//! Telegram chat list and message mapping. No TDLib types and no UI calls.
//!
//! The live client translates TDLib updates into these records on the worker.
//! Default builds test the same rules without linking TDLib. The worker is
//! feature-gated, so the lib build allows this module to look unused.

#![cfg_attr(not(feature = "telegram-tdlib"), allow(dead_code))]

use std::collections::HashMap;

use crate::adapter::{ChatMessage, Conversation, Delivery, ProtocolId};

/// Page size passed to TDLib `loadChats` for the main list.
pub(super) const MAIN_CHAT_LIMIT: i32 = 30;

/// Page size passed to TDLib `getChatHistory`.
pub(super) const HISTORY_LIMIT: i32 = 40;

/// TDLib uses 404 when `loadChats` has already reached the end of the list.
pub(super) const END_OF_CHAT_LIST: i32 = 404;

const PREVIEW_CHARS: usize = 80;

/// Who sent a message, without display strings or phone numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MessageParty {
    User(i64),
    Chat(i64),
}

/// Plain-text view of a TDLib message. Media becomes a short label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct InboxMessage {
    pub chat_id: i64,
    pub message_id: i64,
    pub outgoing: bool,
    pub party: MessageParty,
    pub body: String,
    pub delivery: Delivery,
}

/// What the shell should do after a chat-list mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ChatEffect {
    Upsert(Conversation),
    Remove(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ChatRecord {
    title: String,
    order: i64,
    unread: u32,
    preview: String,
    participant: String,
}

/// Main-list chats keyed by TDLib chat id. `order == 0` is not listed.
#[derive(Debug, Default)]
pub(super) struct ChatDirectory {
    chats: HashMap<i64, ChatRecord>,
}

impl ChatDirectory {
    pub(super) fn new() -> Self {
        Self {
            chats: HashMap::new(),
        }
    }

    pub(super) fn title(&self, chat_id: i64) -> Option<&str> {
        self.chats.get(&chat_id).map(|chat| chat.title.as_str())
    }

    /// Chats currently in the main list. Used to flush updates that arrived
    /// before authorization without showing them early.
    pub(super) fn listed(&self) -> Vec<Conversation> {
        let mut rows: Vec<Conversation> = self
            .chats
            .iter()
            .filter(|(_, chat)| chat.order > 0)
            .map(|(chat_id, chat)| conversation_from(*chat_id, chat))
            .collect();
        rows.sort_by_key(|row| (std::cmp::Reverse(row.order), row.id.clone()));
        rows
    }

    /// Insert or replace a chat. Listed only when `order` is positive.
    pub(super) fn upsert(
        &mut self,
        chat_id: i64,
        title: &str,
        order: i64,
        unread: i32,
        preview: &str,
        participant: &str,
    ) -> Option<ChatEffect> {
        let previous = self.chats.get(&chat_id).map(|chat| chat.order);
        self.chats.insert(
            chat_id,
            ChatRecord {
                title: fallback_title(title),
                order,
                unread: unread_count(unread),
                preview: one_line_preview(preview),
                participant: fallback_title(participant),
            },
        );
        self.effect(chat_id, previous)
    }

    pub(super) fn set_title(&mut self, chat_id: i64, title: &str) -> Option<ChatEffect> {
        let previous = self.ensure(chat_id).order;
        if let Some(chat) = self.chats.get_mut(&chat_id) {
            let title = fallback_title(title);
            if chat.participant == chat.title || chat.participant == "Chat" {
                chat.participant.clone_from(&title);
            }
            chat.title = title;
        }
        self.effect(chat_id, Some(previous))
    }

    pub(super) fn set_main_order(&mut self, chat_id: i64, order: i64) -> Option<ChatEffect> {
        let previous = self.ensure(chat_id).order;
        if let Some(chat) = self.chats.get_mut(&chat_id) {
            chat.order = order;
        }
        self.effect(chat_id, Some(previous))
    }

    pub(super) fn set_unread(&mut self, chat_id: i64, unread: i32) -> Option<ChatEffect> {
        let previous = self.ensure(chat_id).order;
        if let Some(chat) = self.chats.get_mut(&chat_id) {
            chat.unread = unread_count(unread);
        }
        self.effect(chat_id, Some(previous))
    }

    pub(super) fn set_preview(&mut self, chat_id: i64, preview: &str) -> Option<ChatEffect> {
        let previous = self.ensure(chat_id).order;
        if let Some(chat) = self.chats.get_mut(&chat_id) {
            chat.preview = one_line_preview(preview);
        }
        self.effect(chat_id, Some(previous))
    }

    fn ensure(&mut self, chat_id: i64) -> &ChatRecord {
        self.chats.entry(chat_id).or_insert_with(|| ChatRecord {
            title: "Chat".into(),
            order: 0,
            unread: 0,
            preview: String::new(),
            participant: "Chat".into(),
        })
    }

    fn effect(&self, chat_id: i64, previous_order: Option<i64>) -> Option<ChatEffect> {
        let chat = self.chats.get(&chat_id)?;
        let was_listed = previous_order.is_some_and(|order| order > 0);
        if chat.order > 0 {
            return Some(ChatEffect::Upsert(conversation_from(chat_id, chat)));
        }
        if was_listed {
            return Some(ChatEffect::Remove(conversation_id(chat_id)));
        }
        None
    }
}

/// Display names learned from TDLib user updates. Phone numbers are never stored.
#[derive(Debug, Default)]
pub(super) struct NameBook {
    users: HashMap<i64, String>,
}

impl NameBook {
    pub(super) fn new() -> Self {
        Self {
            users: HashMap::new(),
        }
    }

    pub(super) fn remember_user(&mut self, user_id: i64, first_name: &str, last_name: &str) {
        let name = format!("{first_name} {last_name}");
        let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
        if name.is_empty() {
            return;
        }
        self.users.insert(user_id, name);
    }

    pub(super) fn user_label(&self, user_id: i64) -> String {
        self.users
            .get(&user_id)
            .cloned()
            .unwrap_or_else(|| "Telegram user".into())
    }
}

#[must_use]
pub fn conversation_id(chat_id: i64) -> String {
    format!("telegram:{chat_id}")
}

#[must_use]
pub fn parse_telegram_chat_id(conversation_id: &str) -> Option<i64> {
    let rest = conversation_id.strip_prefix("telegram:")?;
    if rest.is_empty() || rest.starts_with('+') {
        return None;
    }
    rest.parse().ok()
}

#[must_use]
pub(super) fn message_id(chat_id: i64, message_id: i64) -> String {
    format!("telegram:{chat_id}:{message_id}")
}

/// Split `telegram:<chat>:<message>` back into TDLib ids.
#[must_use]
pub(super) fn parse_message_id(message_id: &str) -> Option<(i64, i64)> {
    let rest = message_id.strip_prefix("telegram:")?;
    let (chat, message) = rest.split_once(':')?;
    if chat.starts_with('+') || message.starts_with('+') {
        return None;
    }
    Some((chat.parse().ok()?, message.parse().ok()?))
}

#[must_use]
pub(super) fn is_end_of_chat_list(code: i32) -> bool {
    code == END_OF_CHAT_LIST
}

/// TDLib history is newest-first. The thread shows oldest-first.
#[must_use]
pub(super) fn chronological<T>(newest_first: Vec<T>) -> Vec<T> {
    let mut items = newest_first;
    items.reverse();
    items
}

#[must_use]
pub(super) fn to_chat_message(
    message: &InboxMessage,
    names: &NameBook,
    chat_title: Option<&str>,
) -> ChatMessage {
    let sender = if message.outgoing {
        "you".into()
    } else {
        match message.party {
            MessageParty::User(user_id) => names.user_label(user_id),
            MessageParty::Chat(_) => chat_title
                .filter(|title| !title.is_empty())
                .unwrap_or("chat")
                .to_string(),
        }
    };
    ChatMessage {
        protocol: ProtocolId::Telegram,
        conversation_id: conversation_id(message.chat_id),
        id: message_id(message.chat_id, message.message_id),
        sender,
        body: message.body.clone(),
        outbound: message.outgoing,
        delivery: if message.outgoing {
            message.delivery
        } else {
            Delivery::Sent
        },
    }
}

fn conversation_from(chat_id: i64, chat: &ChatRecord) -> Conversation {
    Conversation {
        protocol: ProtocolId::Telegram,
        id: conversation_id(chat_id),
        title: chat.title.clone(),
        participant: chat.participant.clone(),
        preview: chat.preview.clone(),
        unread: chat.unread,
        order: chat.order,
    }
}

fn fallback_title(title: &str) -> String {
    let trimmed = title.trim();
    if trimmed.is_empty() {
        "Chat".into()
    } else {
        trimmed.to_string()
    }
}

fn unread_count(count: i32) -> u32 {
    u32::try_from(count).unwrap_or(0)
}

fn one_line_preview(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = flat.chars();
    let truncated: String = chars.by_ref().take(PREVIEW_CHARS).collect();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_ids_round_trip_and_reject_placeholders() {
        assert_eq!(conversation_id(42), "telegram:42");
        assert_eq!(parse_telegram_chat_id("telegram:42"), Some(42));
        assert_eq!(parse_telegram_chat_id("telegram:-100"), Some(-100));
        assert_eq!(parse_telegram_chat_id("telegram:ready"), None);
        assert_eq!(parse_telegram_chat_id("telegram:saved"), None);
        assert_eq!(parse_telegram_chat_id("telegram:"), None);
        assert_eq!(parse_telegram_chat_id("telegram:+42"), None);
        assert_eq!(parse_telegram_chat_id("slack:1"), None);
        assert_eq!(message_id(42, 7), "telegram:42:7");
    }

    #[test]
    fn main_list_hides_zero_order_and_sorts_by_order_field() {
        let mut directory = ChatDirectory::new();
        assert!(
            directory
                .upsert(1, "Hidden", 0, 3, "nope", "Hidden")
                .is_none()
        );
        let ChatEffect::Upsert(low) = directory
            .upsert(2, "Low", 10, 1, "older", "Low")
            .expect("listed")
        else {
            panic!("expected upsert");
        };
        let ChatEffect::Upsert(high) = directory
            .upsert(3, "High", 90, 4, "newer\nline", "High")
            .expect("listed")
        else {
            panic!("expected upsert");
        };
        assert_eq!(low.order, 10);
        assert_eq!(high.order, 90);
        assert_eq!(high.preview, "newer line");
        assert_eq!(high.unread, 4);
        assert!(high.order > low.order);

        let ChatEffect::Remove(id) = directory.set_main_order(3, 0).expect("removed") else {
            panic!("expected remove");
        };
        assert_eq!(id, "telegram:3");
        assert!(directory.set_main_order(3, 0).is_none());
        let listed = directory.listed();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "telegram:2");
    }

    #[test]
    fn title_unread_and_preview_updates_emit_only_while_listed() {
        let mut directory = ChatDirectory::new();
        assert!(directory.set_title(5, "Later").is_none());
        directory.set_main_order(5, 8);
        let ChatEffect::Upsert(chat) = directory.set_title(5, "Saved").expect("title") else {
            panic!("upsert");
        };
        assert_eq!(chat.title, "Saved");
        let ChatEffect::Upsert(chat) = directory.set_unread(5, -1).expect("unread") else {
            panic!("upsert");
        };
        assert_eq!(chat.unread, 0);
        let ChatEffect::Upsert(chat) = directory
            .set_preview(5, &"word ".repeat(40))
            .expect("preview")
        else {
            panic!("upsert");
        };
        assert!(chat.preview.chars().count() <= PREVIEW_CHARS + 1);
        assert!(chat.preview.ends_with('…'));
        assert!(!chat.preview.contains('\n'));
        let ChatEffect::Upsert(chat) = directory.set_preview(5, "").expect("cleared") else {
            panic!("upsert");
        };
        assert_eq!(chat.preview, "");
        assert_eq!(chat.title, "Saved");
    }

    #[test]
    fn names_drop_blank_users_and_never_need_a_phone() {
        let mut names = NameBook::new();
        names.remember_user(7, "  ", "");
        assert_eq!(names.user_label(7), "Telegram user");
        names.remember_user(7, "Ada", "Lovelace");
        assert_eq!(names.user_label(7), "Ada Lovelace");
        let message = InboxMessage {
            chat_id: 9,
            message_id: 3,
            outgoing: false,
            party: MessageParty::User(7),
            body: "hello".into(),
            delivery: Delivery::Sent,
        };
        let mapped = to_chat_message(&message, &names, Some("Ada"));
        assert_eq!(mapped.sender, "Ada Lovelace");
        assert!(!mapped.outbound);
        assert_eq!(mapped.id, "telegram:9:3");
        let mine = InboxMessage {
            outgoing: true,
            body: "ping".into(),
            ..message
        };
        assert_eq!(to_chat_message(&mine, &names, None).sender, "you");
        assert!(to_chat_message(&mine, &names, None).outbound);
    }

    #[test]
    fn delivery_maps_only_for_outgoing_and_message_ids_parse_back() {
        let names = NameBook::new();
        let failed = InboxMessage {
            chat_id: 9,
            message_id: 4,
            outgoing: true,
            party: MessageParty::User(1),
            body: "ping".into(),
            delivery: Delivery::Failed,
        };
        assert_eq!(
            to_chat_message(&failed, &names, None).delivery,
            Delivery::Failed
        );
        let inbound = InboxMessage {
            outgoing: false,
            delivery: Delivery::Pending,
            ..failed
        };
        assert_eq!(
            to_chat_message(&inbound, &names, None).delivery,
            Delivery::Sent
        );
        assert_eq!(parse_message_id("telegram:9:4"), Some((9, 4)));
        assert_eq!(parse_message_id("telegram:-100:7"), Some((-100, 7)));
        assert_eq!(parse_message_id("telegram:9"), None);
        assert_eq!(parse_message_id("telegram:+9:4"), None);
        assert_eq!(parse_message_id("slack:9:4"), None);
    }

    #[test]
    fn history_reverses_to_oldest_first_and_404_is_the_list_end() {
        assert_eq!(chronological(vec![3, 2, 1]), vec![1, 2, 3]);
        assert!(is_end_of_chat_list(END_OF_CHAT_LIST));
        assert!(!is_end_of_chat_list(400));
        assert_eq!(MAIN_CHAT_LIMIT, 30);
        assert_eq!(HISTORY_LIMIT, 40);
    }
}
