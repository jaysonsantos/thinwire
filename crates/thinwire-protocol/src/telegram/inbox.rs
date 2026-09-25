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

/// Page size for older messages (scroll up in a chat).
pub(super) const OLDER_PAGE_LIMIT: i32 = 50;

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
    pub sent_at: i64,
}

/// What the shell should do after a chat-list mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ChatEffect {
    Upsert(Conversation),
    Remove(String),
}

/// Fields TDLib gives for a new chat.
#[derive(Debug, Clone, Copy)]
pub(super) struct ChatSeed<'a> {
    pub title: &'a str,
    pub order: i64,
    pub unread: i32,
    pub preview: &'a str,
    pub participant: &'a str,
    pub last_at: i64,
    pub is_group: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ChatRecord {
    title: String,
    order: i64,
    unread: u32,
    preview: String,
    participant: String,
    last_at: i64,
    is_group: bool,
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
    pub(super) fn upsert(&mut self, chat_id: i64, seed: ChatSeed<'_>) -> Option<ChatEffect> {
        let previous = self.chats.get(&chat_id).map(|chat| chat.order);
        self.chats.insert(
            chat_id,
            ChatRecord {
                title: fallback_title(seed.title),
                order: seed.order,
                unread: unread_count(seed.unread),
                preview: one_line_preview(seed.preview),
                participant: fallback_title(seed.participant),
                last_at: seed.last_at,
                is_group: seed.is_group,
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

    /// Main-list position from an update that carries the full position list
    /// (for example `updateChatLastMessage`). `None` means the chat left the
    /// main list, for example it was archived: its order drops to zero, so it
    /// leaves the inbox.
    pub(super) fn set_main_position(
        &mut self,
        chat_id: i64,
        main_order: Option<i64>,
    ) -> Option<ChatEffect> {
        self.set_main_order(chat_id, main_order.unwrap_or(0))
    }

    pub(super) fn set_unread(&mut self, chat_id: i64, unread: i32) -> Option<ChatEffect> {
        let previous = self.ensure(chat_id).order;
        if let Some(chat) = self.chats.get_mut(&chat_id) {
            chat.unread = unread_count(unread);
        }
        self.effect(chat_id, Some(previous))
    }

    /// New last message. `at` is Unix seconds; zero clears the time.
    pub(super) fn set_preview(
        &mut self,
        chat_id: i64,
        preview: &str,
        at: i64,
    ) -> Option<ChatEffect> {
        let previous = self.ensure(chat_id).order;
        if let Some(chat) = self.chats.get_mut(&chat_id) {
            chat.preview = one_line_preview(preview);
            chat.last_at = at;
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
            last_at: 0,
            is_group: false,
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

/// What the worker does with a request for older messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OlderStep {
    /// Ask TDLib for the page before this message.
    Fetch,
    /// An earlier empty page reached the start of the chat: no TDLib call.
    AtStart,
    /// The same anchor was already served: no second TDLib call.
    Repeat,
}

/// Per-chat state of older-history requests: the start of the chat once an
/// empty page came, and the last anchor served. The worker runs one TDLib
/// call at a time, so this gate only drops repeats and ends at the start.
#[derive(Debug, Default)]
pub(super) struct OlderHistory {
    at_start: HashMap<i64, bool>,
    last_anchor: HashMap<i64, i64>,
}

impl OlderHistory {
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn begin(&self, chat_id: i64, before: i64) -> OlderStep {
        if self.at_start.get(&chat_id).copied().unwrap_or(false) {
            return OlderStep::AtStart;
        }
        if self.last_anchor.get(&chat_id) == Some(&before) {
            return OlderStep::Repeat;
        }
        OlderStep::Fetch
    }

    /// Record a fetched page. Returns `more`. Only an empty TDLib page marks
    /// the start of the chat. A page with only the anchor records nothing, so
    /// a later request for the same anchor can try again (PR #52 review).
    pub(super) fn finish(&mut self, chat_id: i64, before: i64, page: PageOutcome) -> bool {
        match page {
            PageOutcome::Older(_) => {
                self.last_anchor.insert(chat_id, before);
                self.at_start.insert(chat_id, false);
                true
            }
            PageOutcome::Empty => {
                self.last_anchor.insert(chat_id, before);
                self.at_start.insert(chat_id, true);
                false
            }
            PageOutcome::OnlyAnchor => true,
        }
    }

    /// Drop the pagination state of a chat that left the main list. Its cached
    /// messages go away too, so a later load must not reuse an old anchor or
    /// an old start-of-chat mark (PR #52 review).
    pub(super) fn forget(&mut self, chat_id: i64) {
        self.at_start.remove(&chat_id);
        self.last_anchor.remove(&chat_id);
    }

    /// Messages of this chat were deleted. If they were the whole last page,
    /// the oldest shown message is the old anchor again. Clear the repeat
    /// gate so that request fetches (PR #52 review). The start-of-chat mark
    /// stays: a delete adds no older message.
    pub(super) fn messages_deleted(&mut self, chat_id: i64) {
        self.last_anchor.remove(&chat_id);
    }

    /// Forget a chat when `effect` removes it from the list.
    pub(super) fn follow(&mut self, effect: Option<&ChatEffect>) {
        if let Some(ChatEffect::Remove(id)) = effect
            && let Some(chat_id) = parse_telegram_chat_id(id)
        {
            self.forget(chat_id);
        }
    }

    /// `more` for an answer without a TDLib call.
    #[must_use]
    pub(super) fn more(&self, chat_id: i64) -> bool {
        !self.at_start.get(&chat_id).copied().unwrap_or(false)
    }
}

/// How an older-history request ends: `more`, and the note for this chat.
/// A failed page is a note for the chat, not an account `Error` status, and
/// it is not the start of the chat, so a later scroll can try again. A
/// success sends no note, which clears an earlier one (#57).
pub(super) fn older_end(result: Result<bool, i32>) -> (bool, Option<String>) {
    match result {
        Ok(more) => (more, None),
        Err(code) => (
            true,
            Some(format!("Could not load older messages (TDLib {code}).")),
        ),
    }
}

/// What one TDLib history page held, relative to the anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PageOutcome {
    /// This many messages older than the anchor.
    Older(usize),
    /// Only the anchor itself (TDLib can return a short page before it has
    /// the older ones): nothing new yet, retryable. Not the start of the chat.
    OnlyAnchor,
    /// TDLib returned no message at all: the start of the chat.
    Empty,
}

/// Classify a page by its raw size (before the anchor is removed) and the
/// number of messages older than the anchor.
#[must_use]
pub(super) const fn page_outcome(raw_len: usize, older_len: usize) -> PageOutcome {
    if older_len > 0 {
        PageOutcome::Older(older_len)
    } else if raw_len == 0 {
        PageOutcome::Empty
    } else {
        PageOutcome::OnlyAnchor
    }
}

/// Keep only messages older than the anchor, oldest first. TDLib may return
/// the anchor itself; it is already on screen.
#[must_use]
pub(super) fn older_than<T>(newest_first: Vec<T>, before: i64, id: impl Fn(&T) -> i64) -> Vec<T> {
    let mut older: Vec<T> = newest_first
        .into_iter()
        .filter(|item| id(item) < before)
        .collect();
    older.reverse();
    older
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
        sent_at: message.sent_at,
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
        last_at: chat.last_at,
        is_group: chat.is_group,
        writable: true,
        placeholder: false,
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

    fn seed<'a>(title: &'a str, order: i64, unread: i32, preview: &'a str) -> ChatSeed<'a> {
        ChatSeed {
            title,
            order,
            unread,
            preview,
            participant: title,
            last_at: 0,
            is_group: false,
        }
    }

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
        assert!(directory.upsert(1, seed("Hidden", 0, 3, "nope")).is_none());
        let ChatEffect::Upsert(low) = directory
            .upsert(2, seed("Low", 10, 1, "older"))
            .expect("listed")
        else {
            panic!("expected upsert");
        };
        let ChatEffect::Upsert(high) = directory
            .upsert(3, seed("High", 90, 4, "newer\nline"))
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
    fn upsert_keeps_the_last_message_time_and_the_group_flag() {
        let mut directory = ChatDirectory::new();
        let ChatEffect::Upsert(chat) = directory
            .upsert(
                7,
                ChatSeed {
                    last_at: 1_700_000_000,
                    is_group: true,
                    ..seed("Team", 5, 0, "hi")
                },
            )
            .expect("listed")
        else {
            panic!("expected upsert");
        };
        assert_eq!(chat.last_at, 1_700_000_000);
        assert!(chat.is_group);
        let ChatEffect::Upsert(chat) = directory
            .set_preview(7, "later", 1_700_000_600)
            .expect("preview")
        else {
            panic!("expected upsert");
        };
        assert_eq!(chat.last_at, 1_700_000_600);
        assert!(chat.is_group, "a preview update keeps the chat kind");
    }

    #[test]
    fn a_last_message_update_without_a_main_position_removes_the_chat() {
        let mut directory = ChatDirectory::new();
        directory.upsert(4, seed("Archived later", 50, 0, "hi"));
        // A fake updateChatLastMessage: new text, positions without Main.
        let ChatEffect::Remove(id) = directory
            .set_main_position(4, None)
            .expect("the chat leaves the main list")
        else {
            panic!("expected remove");
        };
        assert_eq!(id, "telegram:4");
        assert!(
            directory
                .set_preview(4, "later text", 1_700_000_000)
                .is_none(),
            "a preview does not bring it back"
        );
        assert!(directory.listed().is_empty());
        let ChatEffect::Upsert(chat) = directory
            .set_main_position(4, Some(60))
            .expect("back in the main list")
        else {
            panic!("expected upsert");
        };
        assert_eq!(chat.order, 60);
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
            .set_preview(5, &"word ".repeat(40), 1_700_000_000)
            .expect("preview")
        else {
            panic!("upsert");
        };
        assert!(chat.preview.chars().count() <= PREVIEW_CHARS + 1);
        assert!(chat.preview.ends_with('…'));
        assert!(!chat.preview.contains('\n'));
        assert_eq!(chat.last_at, 1_700_000_000);
        let ChatEffect::Upsert(chat) = directory.set_preview(5, "", 0).expect("cleared") else {
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
            sent_at: 1_700_000_000,
        };
        let mapped = to_chat_message(&message, &names, Some("Ada"));
        assert_eq!(mapped.sender, "Ada Lovelace");
        assert_eq!(mapped.sent_at, 1_700_000_000);
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
            sent_at: 0,
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
    fn older_pages_merge_oldest_first_and_stop_at_the_start() {
        // TDLib gives newest first and can include the anchor (id 50).
        let page = older_than(vec![50, 49, 48, 47], 50, |id| *id);
        assert_eq!(page, vec![47, 48, 49], "oldest first, anchor dropped");
        assert_eq!(OLDER_PAGE_LIMIT, 50);

        let mut gate = OlderHistory::new();
        assert_eq!(gate.begin(1, 50), OlderStep::Fetch);
        assert!(
            gate.finish(1, 50, page_outcome(4, page.len())),
            "more can load"
        );
        assert_eq!(
            gate.begin(1, 50),
            OlderStep::Repeat,
            "the same anchor is not fetched twice"
        );
        assert_eq!(gate.begin(1, 47), OlderStep::Fetch, "the next page");
        assert!(
            !gate.finish(1, 47, page_outcome(0, 0)),
            "an empty page is the start of the chat"
        );
        assert_eq!(gate.begin(1, 12), OlderStep::AtStart);
        assert!(!gate.more(1));
        assert_eq!(
            gate.begin(2, 9),
            OlderStep::Fetch,
            "other chats are separate"
        );
        assert!(gate.more(2));
    }

    #[test]
    fn an_anchor_only_page_is_retryable_not_the_start_of_the_chat() {
        // TDLib returned a short page that holds only the anchor (id 50).
        let page = older_than(vec![50], 50, |id| *id);
        assert!(page.is_empty());
        let outcome = page_outcome(1, page.len());
        assert_eq!(outcome, PageOutcome::OnlyAnchor);

        let mut gate = OlderHistory::new();
        assert_eq!(gate.begin(1, 50), OlderStep::Fetch);
        assert!(gate.finish(1, 50, outcome), "more = true: nothing new yet");
        assert!(gate.more(1));
        assert_eq!(
            gate.begin(1, 50),
            OlderStep::Fetch,
            "the same anchor can be asked again later"
        );
        assert!(
            gate.finish(1, 50, page_outcome(3, 2)),
            "then the older page comes"
        );
        assert_eq!(page_outcome(0, 0), PageOutcome::Empty);
        assert!(
            !gate.finish(1, 48, PageOutcome::Empty),
            "only an empty page ends it"
        );
        assert_eq!(gate.begin(1, 10), OlderStep::AtStart);
    }

    #[test]
    fn a_chat_that_leaves_the_main_list_loses_its_older_history_state() {
        let mut directory = ChatDirectory::new();
        directory.upsert(3, seed("Ada", 9, 0, ""));
        let mut gate = OlderHistory::new();
        assert!(gate.finish(3, 50, PageOutcome::Older(2)));
        assert!(!gate.finish(3, 20, PageOutcome::Empty));
        assert!(gate.finish(4, 70, PageOutcome::Older(1)));
        assert_eq!(gate.begin(3, 20), OlderStep::AtStart);

        // Archived: the chat and its cached messages leave the inbox.
        let removed = directory.set_main_order(3, 0);
        assert!(matches!(removed, Some(ChatEffect::Remove(_))));
        gate.follow(removed.as_ref());
        assert!(gate.more(3), "no stale start-of-chat mark");
        assert_eq!(gate.begin(3, 20), OlderStep::Fetch);
        assert_eq!(gate.begin(3, 50), OlderStep::Fetch, "no stale anchor");
        assert_eq!(
            gate.begin(4, 70),
            OlderStep::Repeat,
            "other chats keep theirs"
        );

        // An update that keeps the chat listed forgets nothing.
        let mut gate = OlderHistory::new();
        assert!(gate.finish(3, 50, PageOutcome::Older(2)));
        let kept = directory.set_main_order(3, 5);
        assert!(matches!(kept, Some(ChatEffect::Upsert(_))));
        gate.follow(kept.as_ref());
        gate.follow(None);
        assert_eq!(gate.begin(3, 50), OlderStep::Repeat);
    }

    #[test]
    fn a_deleted_last_page_does_not_consume_its_anchor() {
        let mut gate = OlderHistory::new();
        // Anchor 50 loaded 47..=49; then all three are deleted, so the UI
        // asks again from 50.
        assert!(gate.finish(1, 50, PageOutcome::Older(3)));
        assert!(gate.finish(2, 80, PageOutcome::Older(1)));
        assert_eq!(gate.begin(1, 50), OlderStep::Repeat);
        gate.messages_deleted(1);
        assert_eq!(gate.begin(1, 50), OlderStep::Fetch, "not consumed");
        assert_eq!(
            gate.begin(2, 80),
            OlderStep::Repeat,
            "other chats keep theirs"
        );

        // The start of the chat stays the start.
        assert!(!gate.finish(1, 50, PageOutcome::Empty));
        gate.messages_deleted(1);
        assert_eq!(gate.begin(1, 50), OlderStep::AtStart);
    }

    #[test]
    fn a_failed_older_page_is_a_chat_note_and_a_success_clears_it() {
        let (more, note) = older_end(Err(500));
        assert!(more, "a later scroll can try again");
        assert_eq!(
            note.as_deref(),
            Some("Could not load older messages (TDLib 500).")
        );
        assert_eq!(older_end(Ok(true)), (true, None), "success clears the note");
        assert_eq!(older_end(Ok(false)), (false, None));

        let src = include_str!("tdlib.rs");
        let load = &src[src.find("async fn load_older(").expect("load_older")..];
        let load = &load[..load.find("\n}\n").expect("end")];
        assert!(
            !load.contains("emit_status("),
            "no account error for one page"
        );
        assert!(load.contains("older_end(Err(error.code))"));
        assert!(load.contains("older_end(Ok(more))"));
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
