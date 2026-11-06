// SPDX-License-Identifier: AGPL-3.0-only
#![cfg_attr(not(feature = "signal-local"), allow(dead_code))]
//! Replies, reactions, and deletes of Signal messages (wire version 6).
//!
//! A row id is `<sent millis>:<author>`. The author is the ACI of the
//! sender, or `me` for a message that this device sent.

use std::collections::HashMap;

use thinwire_protocol::Reaction;

/// Who reacted, for a reaction of this account.
pub(crate) const ME: &str = "me";

/// Messages with reactions that one run keeps, at most.
const BOOK_MESSAGES: usize = 4096;

/// The sent time and the author of a row id. `None` as the author is this
/// account: the caller puts in its own ACI.
pub(crate) fn target_of(message_id: &str) -> Option<(u64, Option<&str>)> {
    let (millis, author) = message_id.split_once(':')?;
    let millis = millis.parse().ok()?;
    if author.is_empty() {
        return None;
    }
    Some((millis, (author != ME).then_some(author)))
}

/// The row ids that a message of `author` at `millis` can have. A message
/// of this account has two: the one of a send of this device, and the one
/// of the store.
pub(crate) fn row_ids(millis: u64, author: &str, own_aci: &str) -> Vec<String> {
    let mut ids = vec![format!("{millis}:{author}")];
    if author == own_aci {
        ids.push(format!("{millis}:{ME}"));
    }
    ids
}

/// The reactions of the messages that this run saw: for each row id, who
/// reacted and the emoji. One reaction for each person.
#[derive(Debug, Default)]
pub(crate) struct ReactionBook {
    by_message: HashMap<String, Vec<(String, String)>>,
}

impl ReactionBook {
    /// Set or remove the reaction of `reactor` on `message_id`. Returns the
    /// counts of the message now.
    pub(crate) fn apply(
        &mut self,
        message_id: &str,
        reactor: &str,
        emoji: &str,
        remove: bool,
    ) -> Vec<Reaction> {
        if !self.by_message.contains_key(message_id) && self.by_message.len() >= BOOK_MESSAGES {
            // The book forgets one message. Its row keeps the counts it shows.
            if let Some(old) = self.by_message.keys().next().cloned() {
                self.by_message.remove(&old);
            }
        }
        let reactions = self.by_message.entry(message_id.to_owned()).or_default();
        reactions.retain(|(who, _)| who != reactor);
        if !remove && !emoji.is_empty() {
            reactions.push((reactor.to_owned(), emoji.to_owned()));
        }
        counts(reactions)
    }
}

/// The count of each emoji, in the order of the first person who chose it.
fn counts(reactions: &[(String, String)]) -> Vec<Reaction> {
    let mut out: Vec<Reaction> = Vec::new();
    for (who, emoji) in reactions {
        match out.iter_mut().find(|reaction| &reaction.emoji == emoji) {
            Some(reaction) => {
                reaction.count = reaction.count.saturating_add(1);
                reaction.mine |= who == ME;
            }
            None => out.push(Reaction {
                emoji: emoji.clone(),
                count: 1,
                mine: who == ME,
            }),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_row_id_names_its_time_and_author() {
        assert_eq!(target_of("1700:abc"), Some((1700, Some("abc"))));
        assert_eq!(target_of("1700:me"), Some((1700, None)));
        assert_eq!(target_of("x:abc"), None);
        assert_eq!(target_of("1700:"), None);
        assert_eq!(row_ids(5, "own", "own"), vec!["5:own", "5:me"]);
        assert_eq!(row_ids(5, "other", "own"), vec!["5:other"]);
    }

    #[test]
    fn each_person_has_one_reaction() {
        let mut book = ReactionBook::default();
        book.apply("1:a", "b", "👍", false);
        let shown = book.apply("1:a", ME, "👍", false);
        assert_eq!((shown[0].count, shown[0].mine), (2, true));
        let shown = book.apply("1:a", "b", "❤️", false);
        assert_eq!(shown.len(), 2, "b moved to another emoji");
        let shown = book.apply("1:a", ME, "", true);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].emoji, "❤️");
    }
}
