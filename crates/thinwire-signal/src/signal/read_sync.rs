// SPDX-License-Identifier: AGPL-3.0-only
#![cfg_attr(not(feature = "signal-local"), allow(dead_code))]
//! Messages that another device of the account read.
//!
//! When the user reads a chat on the phone, the phone sends a sync message
//! with a list of read messages. Each item has the sender and the sent time
//! of one message, and no chat. This book keeps the unread live messages of
//! this run (chat, sender, sent time), so a read item finds its chat. A read
//! marks that message and each older unread message of the chat as read.

use std::collections::HashMap;

/// Unread messages that one chat keeps, at most. The oldest go first.
const CHAT_MESSAGES: usize = 1000;

/// The unread live messages of this run, for each chat.
#[derive(Debug, Default)]
pub(crate) struct ReadBook {
    chats: HashMap<String, Vec<(String, u64)>>,
}

impl ReadBook {
    /// An unread message of `sender`, sent at `millis`, came in `chat`.
    pub(crate) fn note(&mut self, chat: &str, sender: &str, millis: u64) {
        let unread = self.chats.entry(chat.to_string()).or_default();
        if unread.len() >= CHAT_MESSAGES {
            unread.remove(0);
        }
        unread.push((sender.to_string(), millis));
    }

    /// The chat has no unread message now.
    pub(crate) fn clear(&mut self, chat: &str) {
        self.chats.remove(chat);
    }

    /// Another device read the message of `sender` sent at `millis`.
    /// Returns the chat and the count of its messages that stay unread, or
    /// `None` when no unread message of this run matches.
    pub(crate) fn read(&mut self, sender: &str, millis: u64) -> Option<(String, u32)> {
        let chat = self
            .chats
            .iter()
            .find(|(_, unread)| {
                unread
                    .iter()
                    .any(|(from, sent)| from == sender && *sent == millis)
            })
            .map(|(chat, _)| chat.clone())?;
        let unread = self.chats.get_mut(&chat)?;
        unread.retain(|(_, sent)| *sent > millis);
        let left = u32::try_from(unread.len()).unwrap_or(u32::MAX);
        if left == 0 {
            self.chats.remove(&chat);
        }
        Some((chat, left))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_on_the_phone_clears_that_message_and_the_older_ones() {
        let mut book = ReadBook::default();
        book.note("ada", "ada-aci", 10);
        book.note("ada", "ada-aci", 20);
        book.note("ada", "ada-aci", 30);
        assert_eq!(book.read("ada-aci", 20), Some(("ada".to_string(), 1)));
        assert_eq!(book.read("ada-aci", 30), Some(("ada".to_string(), 0)));
        assert_eq!(book.read("ada-aci", 30), None, "the chat has no unread now");
    }

    #[test]
    fn a_read_finds_the_group_of_the_message() {
        let mut book = ReadBook::default();
        book.note("group-1", "bob-aci", 5);
        book.note("group-1", "ada-aci", 7);
        book.note("ada", "ada-aci", 6);
        assert_eq!(book.read("bob-aci", 5), Some(("group-1".to_string(), 1)));
        assert_eq!(book.read("ada-aci", 7), Some(("group-1".to_string(), 0)));
        assert_eq!(book.read("ada-aci", 6), Some(("ada".to_string(), 0)));
    }

    #[test]
    fn an_unknown_read_changes_nothing() {
        let mut book = ReadBook::default();
        book.note("ada", "ada-aci", 10);
        assert_eq!(book.read("ada-aci", 11), None);
        assert_eq!(book.read("bob-aci", 10), None);
        assert_eq!(book.read("ada-aci", 10), Some(("ada".to_string(), 0)));
    }

    #[test]
    fn a_clear_forgets_the_chat() {
        let mut book = ReadBook::default();
        book.note("ada", "ada-aci", 10);
        book.clear("ada");
        assert_eq!(book.read("ada-aci", 10), None);
    }

    #[test]
    fn a_chat_keeps_only_its_newest_unread_messages() {
        let mut book = ReadBook::default();
        for millis in 0..=CHAT_MESSAGES as u64 {
            book.note("ada", "ada-aci", millis);
        }
        assert_eq!(book.read("ada-aci", 0), None, "the oldest message left");
        assert_eq!(
            book.read("ada-aci", 1),
            Some(("ada".to_string(), (CHAT_MESSAGES - 1) as u32))
        );
    }
}
