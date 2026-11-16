//! The search of message text (#514, #535): which TDLib request a command
//! needs, and the result row of one found message.
//!
//! The query, the text, the names, and the ids are user data. Nothing here
//! goes to a log line.

use super::inbox::{self, InboxMessage, NameBook};
use crate::{FOUND_MESSAGES_LIMIT, FoundMessage};

/// The most messages that one request asks TDLib for.
pub(super) const LIMIT: i32 = FOUND_MESSAGES_LIMIT as i32;

/// Where one search looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Scope {
    /// Every chat of the main chat list: TDLib `searchMessages`.
    Account,
    /// One chat: TDLib `searchChatMessages`.
    Chat(i64),
}

/// The scope of a `SearchMessages` command. `None` when the command names a
/// chat that is not a Telegram chat, or when the query is empty: TDLib
/// gives the newest messages for an empty query, and that is not a search.
pub(super) fn scope(conversation_id: Option<&str>, query: &str) -> Option<Scope> {
    if query.trim().is_empty() {
        return None;
    }
    match conversation_id {
        None => Some(Scope::Account),
        Some(id) => inbox::parse_telegram_chat_id(id).map(Scope::Chat),
    }
}

/// The result row of one found message. The sender, the ids, and the topic
/// are those of the same message in a history page (`to_chat_message`), so
/// the core finds the message in its thread. The text is cut around the
/// match.
pub(super) fn found(
    message: &InboxMessage,
    names: &NameBook,
    chat_title: Option<&str>,
    query: &str,
) -> FoundMessage {
    let row = inbox::to_chat_message(message, names, chat_title);
    FoundMessage {
        conversation_id: row.conversation_id,
        message_id: row.id,
        topic: row.topic,
        sender: row.sender,
        outbound: row.outbound,
        sent_at: row.sent_at,
        text: FoundMessage::cut_text(&row.body, query),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telegram::inbox::MessageParty;
    use crate::{Delivery, FOUND_TEXT_CHARS, MessageTopic};

    fn message(body: &str) -> InboxMessage {
        InboxMessage {
            chat_id: 42,
            message_id: 7,
            outgoing: false,
            party: MessageParty::User(5),
            body: body.to_owned(),
            delivery: Delivery::Sent,
            sent_at: 1_700_000_000,
            topic: None,
        }
    }

    /// A command with no chat searches the account, and a command with a
    /// chat searches that chat only.
    #[test]
    fn a_command_with_a_chat_searches_that_chat_only() {
        assert_eq!(scope(None, "tea"), Some(Scope::Account));
        let chat = inbox::conversation_id(42);
        assert_eq!(scope(Some(&chat), "tea"), Some(Scope::Chat(42)));
        assert_eq!(scope(Some("slack:C1"), "tea"), None, "not a Telegram chat");
        assert_eq!(scope(None, "  "), None, "an empty query is not a search");
        assert_eq!(scope(Some(&chat), ""), None);
    }

    /// The row has the ids that a history page gives the same message.
    #[test]
    fn a_found_message_has_the_ids_of_its_history_row() {
        let mut names = NameBook::new();
        names.remember_user(5, "Ada", "Park");
        let mut source = message("Tea at five?");
        source.topic = Some(MessageTopic::Forum(3));
        let row = found(&source, &names, Some("Book club"), "tea");
        assert_eq!(row.conversation_id, inbox::conversation_id(42));
        assert_eq!(row.message_id, inbox::message_id(42, 7));
        assert_eq!(
            inbox::parse_message_id(&row.message_id),
            Some((42, 7)),
            "the id parses back"
        );
        assert_eq!(row.topic, Some(MessageTopic::Forum(3)));
        assert_eq!(row.sender, "Ada Park");
        assert!(!row.outbound);
        assert_eq!(row.sent_at, 1_700_000_000);
        assert_eq!(row.text, "Tea at five?");
    }

    /// A channel post has the chat name as its sender, and a message of
    /// this account is outbound.
    #[test]
    fn a_post_and_an_own_message_have_their_sender() {
        let names = NameBook::new();
        let mut post = message("News");
        post.party = MessageParty::Chat(42);
        assert_eq!(found(&post, &names, Some("Daily"), "news").sender, "Daily");
        let mut own = message("News");
        own.outgoing = true;
        assert!(found(&own, &names, None, "news").outbound);
    }

    /// A long text keeps the part with the match.
    #[test]
    fn a_long_text_is_cut_around_the_match() {
        let body = format!("{} needle {}", "a ".repeat(600), "b ".repeat(600));
        let row = found(&message(&body), &NameBook::new(), None, "NEEDLE");
        assert!(row.text.chars().count() <= FOUND_TEXT_CHARS);
        assert!(row.text.contains("needle"));
    }
}
