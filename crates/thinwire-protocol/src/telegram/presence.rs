//! The status line of each Telegram chat: "online" or the last time of a
//! person, "bot", and the member count of a group or a channel.
//!
//! TDLib sends the status of a user and the size of a group apart from the
//! chat. This book joins them to chats. It makes no request and holds no
//! name, text, or phone number.

use std::collections::HashMap;

use crate::ChatStatus;

/// What a chat is, for its status line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Peer {
    /// A private or secret chat with this user.
    User(i64),
    BasicGroup(i64),
    Supergroup {
        id: i64,
        channel: bool,
    },
}

/// One status to publish: the TDLib chat id and its new status. `None`
/// removes the line.
pub(super) type Change = (i64, Option<ChatStatus>);

#[derive(Debug, Default)]
pub(super) struct Presence {
    /// The user id of this account. Its own chat (Saved Messages) has no line.
    me: Option<i64>,
    peers: HashMap<i64, Peer>,
    users: HashMap<i64, ChatStatus>,
    basic_groups: HashMap<i64, u32>,
    supergroups: HashMap<i64, u32>,
    /// What each chat published last, so an equal status is not sent again.
    sent: HashMap<i64, ChatStatus>,
}

impl Presence {
    pub(super) fn set_me(&mut self, user_id: i64) -> Vec<Change> {
        self.me = Some(user_id);
        self.refresh(|peer| peer == Peer::User(user_id))
    }

    /// A chat that TDLib named. Its status goes out when one is known.
    pub(super) fn note_chat(&mut self, chat_id: i64, peer: Peer) -> Vec<Change> {
        self.peers.insert(chat_id, peer);
        self.refresh_chat(chat_id).into_iter().collect()
    }

    pub(super) fn forget_chat(&mut self, chat_id: i64) {
        self.peers.remove(&chat_id);
        self.sent.remove(&chat_id);
    }

    /// The new status of a user. `None` is a user with no line (a deleted
    /// account, or the service account of Telegram).
    pub(super) fn user(&mut self, user_id: i64, status: Option<ChatStatus>) -> Vec<Change> {
        match status {
            Some(status) => self.users.insert(user_id, status),
            None => self.users.remove(&user_id),
        };
        self.refresh(|peer| peer == Peer::User(user_id))
    }

    /// A user that is a bot keeps "bot": a later status update does not
    /// replace it.
    pub(super) fn user_status(&mut self, user_id: i64, status: ChatStatus) -> Vec<Change> {
        if self.users.get(&user_id) == Some(&ChatStatus::Bot) {
            return Vec::new();
        }
        self.user(user_id, Some(status))
    }

    pub(super) fn basic_group(&mut self, id: i64, members: i32) -> Vec<Change> {
        self.basic_groups.insert(id, count(members));
        self.refresh(|peer| peer == Peer::BasicGroup(id))
    }

    pub(super) fn supergroup(&mut self, id: i64, members: i32) -> Vec<Change> {
        // TDLib sends 0 when it does not know the count. A known count stays.
        if members <= 0 && self.supergroups.contains_key(&id) {
            return Vec::new();
        }
        self.supergroups.insert(id, count(members));
        self.refresh(|peer| matches!(peer, Peer::Supergroup { id: peer_id, .. } if peer_id == id))
    }

    /// The status of every chat that has one. The worker sends it when the
    /// account links: before that, no status goes out.
    pub(super) fn current(&self) -> Vec<(i64, ChatStatus)> {
        self.sent
            .iter()
            .map(|(chat_id, status)| (*chat_id, *status))
            .collect()
    }

    fn status_of(&self, peer: Peer) -> Option<ChatStatus> {
        match peer {
            Peer::User(id) if self.me == Some(id) => None,
            Peer::User(id) => self.users.get(&id).copied(),
            Peer::BasicGroup(id) => members(self.basic_groups.get(&id).copied(), false),
            Peer::Supergroup { id, channel } => {
                members(self.supergroups.get(&id).copied(), channel)
            }
        }
    }

    fn refresh(&mut self, wanted: impl Fn(Peer) -> bool) -> Vec<Change> {
        let chats: Vec<i64> = self
            .peers
            .iter()
            .filter(|(_, peer)| wanted(**peer))
            .map(|(chat_id, _)| *chat_id)
            .collect();
        chats
            .into_iter()
            .filter_map(|chat_id| self.refresh_chat(chat_id))
            .collect()
    }

    fn refresh_chat(&mut self, chat_id: i64) -> Option<Change> {
        let status = self.status_of(*self.peers.get(&chat_id)?);
        if self.sent.get(&chat_id).copied() == status {
            return None;
        }
        match status {
            Some(status) => self.sent.insert(chat_id, status),
            None => self.sent.remove(&chat_id),
        };
        Some((chat_id, status))
    }
}

fn count(members: i32) -> u32 {
    u32::try_from(members).unwrap_or(0)
}

fn members(count: Option<u32>, channel: bool) -> Option<ChatStatus> {
    match count? {
        0 => None,
        count if channel => Some(ChatStatus::Subscribers(count)),
        count => Some(ChatStatus::Members(count)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_user_status_goes_to_the_chat_of_that_user_one_time() {
        let mut book = Presence::default();
        // The status comes before the chat: nothing goes out yet.
        assert!(book.user_status(7, ChatStatus::Online).is_empty());
        assert_eq!(
            book.note_chat(7, Peer::User(7)),
            vec![(7, Some(ChatStatus::Online))]
        );
        // The same status again changes nothing.
        assert!(book.user_status(7, ChatStatus::Online).is_empty());
        assert_eq!(
            book.user_status(7, ChatStatus::LastSeen(100)),
            vec![(7, Some(ChatStatus::LastSeen(100)))]
        );
        // A user with no chat publishes nothing.
        assert!(book.user_status(8, ChatStatus::Online).is_empty());
    }

    #[test]
    fn a_secret_chat_has_the_status_of_its_user() {
        let mut book = Presence::default();
        book.note_chat(7, Peer::User(7));
        book.note_chat(-900, Peer::User(7));
        let mut changes = book.user_status(7, ChatStatus::Recently);
        changes.sort_by_key(|change| change.0);
        assert_eq!(
            changes,
            vec![
                (-900, Some(ChatStatus::Recently)),
                (7, Some(ChatStatus::Recently))
            ]
        );
    }

    #[test]
    fn the_own_chat_and_a_removed_user_have_no_line() {
        let mut book = Presence::default();
        book.note_chat(1, Peer::User(1));
        assert_eq!(
            book.user_status(1, ChatStatus::Online),
            vec![(1, Some(ChatStatus::Online))]
        );
        assert_eq!(book.set_me(1), vec![(1, None)]);
        assert!(book.user_status(1, ChatStatus::Recently).is_empty());

        book.note_chat(2, Peer::User(2));
        book.user_status(2, ChatStatus::Online);
        assert_eq!(book.user(2, None), vec![(2, None)]);
    }

    #[test]
    fn a_bot_stays_a_bot() {
        let mut book = Presence::default();
        book.note_chat(5, Peer::User(5));
        assert_eq!(
            book.user(5, Some(ChatStatus::Bot)),
            vec![(5, Some(ChatStatus::Bot))]
        );
        assert!(book.user_status(5, ChatStatus::LongAgo).is_empty());
    }

    #[test]
    fn a_group_and_a_channel_show_their_size() {
        let mut book = Presence::default();
        book.note_chat(-10, Peer::BasicGroup(10));
        assert_eq!(
            book.basic_group(10, 4),
            vec![(-10, Some(ChatStatus::Members(4)))]
        );
        book.note_chat(
            -20,
            Peer::Supergroup {
                id: 20,
                channel: true,
            },
        );
        assert!(book.supergroup(20, 0).is_empty());
        assert_eq!(
            book.supergroup(20, 1200),
            vec![(-20, Some(ChatStatus::Subscribers(1200)))]
        );
        // An unknown count does not remove a known one.
        assert!(book.supergroup(20, 0).is_empty());
    }

    #[test]
    fn a_chat_that_comes_back_gets_its_status_again() {
        let mut book = Presence::default();
        book.note_chat(7, Peer::User(7));
        book.user_status(7, ChatStatus::Online);
        book.forget_chat(7);
        assert_eq!(
            book.note_chat(7, Peer::User(7)),
            vec![(7, Some(ChatStatus::Online))]
        );
    }
}
