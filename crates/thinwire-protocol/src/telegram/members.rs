//! The people of a Telegram group, for the name completion after "@" in the
//! compose field.
//!
//! A person with a public username is mentioned as "@username": Telegram
//! finds that mention in the text by itself. A person with no username is
//! mentioned as "@" and the name. Telegram does not know that text, so this
//! book keeps the user id of each such person, and a send adds a text
//! mention with that id on each place the text has the name.
//!
//! Nothing here goes to a log line.

use std::collections::HashMap;

use crate::ChatMember;

/// The most members one request asks for. It is the limit of TDLib
/// `searchChatMembers` for a supergroup.
pub(super) const MEMBER_LIMIT: i32 = 200;

/// The list entry of one person. `None` for a person with no name and no
/// username.
pub(super) fn member(
    first_name: &str,
    last_name: &str,
    username: Option<&str>,
) -> Option<ChatMember> {
    let name = format!("{first_name} {last_name}");
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    let handle = username
        .map(str::trim)
        .filter(|handle| !handle.is_empty())
        .map(str::to_owned);
    let name = if name.is_empty() {
        handle.clone()?
    } else {
        name
    };
    Some(ChatMember { name, handle })
}

/// The people whose mention text names one person only. A person with no
/// username leaves the list when another person has the same mention text:
/// a text with that name cannot tell which of them it means, and a wrong
/// person gets no notification from this app. Each entry has the user id.
pub(super) fn clear_mentions(people: Vec<(ChatMember, i64)>) -> Vec<(ChatMember, i64)> {
    let mut count: HashMap<String, usize> = HashMap::new();
    for (person, _) in &people {
        *count.entry(person.mention()).or_default() += 1;
    }
    people
        .into_iter()
        .filter(|(person, _)| person.handle.is_some() || count[&person.mention()] == 1)
        .collect()
}

/// A text mention to add to a sent text: a range in UTF-16 code units, as
/// TDLib counts, and the user it names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TextMention {
    pub offset: i32,
    pub length: i32,
    pub user_id: i64,
}

/// For each chat, the people with no username: the mention text and the
/// user id.
#[derive(Debug, Default)]
pub(super) struct MemberBook {
    chats: HashMap<i64, Vec<(String, i64)>>,
}

impl MemberBook {
    /// The people of `chat_id` who have no username. A new list takes the
    /// place of the old one.
    pub(super) fn set(&mut self, chat_id: i64, mut people: Vec<(String, i64)>) {
        // The longest text first, so "@Ada Lovelace" wins over "@Ada".
        people.sort_by_key(|person| std::cmp::Reverse(person.0.len()));
        self.chats.insert(chat_id, people);
    }

    /// The text mentions of `text` for a send to `chat_id`. A mention text
    /// counts where it starts the text or follows white space, and where a
    /// letter, a digit, or "_" does not follow it. [`clear_mentions`] keeps
    /// two people with the same name out of the book. If the book has such
    /// a pair, the first one gets the mention.
    pub(super) fn mentions(&self, chat_id: i64, text: &str) -> Vec<TextMention> {
        let Some(people) = self.chats.get(&chat_id) else {
            return Vec::new();
        };
        if !text.contains('@') {
            return Vec::new();
        }
        // Byte ranges that already have a mention.
        let mut taken: Vec<(usize, usize, i64)> = Vec::new();
        for (mention, user_id) in people {
            for (start, found) in text.match_indices(mention.as_str()) {
                let end = start + found.len();
                let starts_word = text[..start]
                    .chars()
                    .next_back()
                    .is_none_or(char::is_whitespace);
                let ends_word = text[end..]
                    .chars()
                    .next()
                    .is_none_or(|next| !(next.is_alphanumeric() || next == '_'));
                let free = taken
                    .iter()
                    .all(|(from, to, _)| end <= *from || *to <= start);
                if starts_word && ends_word && free {
                    taken.push((start, end, *user_id));
                }
            }
        }
        taken.sort_by_key(|(start, _, _)| *start);
        taken
            .into_iter()
            .filter_map(|(start, end, user_id)| {
                Some(TextMention {
                    offset: i32::try_from(text[..start].encode_utf16().count()).ok()?,
                    length: i32::try_from(text[start..end].encode_utf16().count()).ok()?,
                    user_id,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_member_has_a_joined_name_and_its_username() {
        let ada = member(" Ada ", "Lovelace", Some("ada_l")).expect("member");
        assert_eq!(ada.name, "Ada Lovelace");
        assert_eq!(ada.handle.as_deref(), Some("ada_l"));
        assert_eq!(ada.mention(), "@ada_l");

        let milo = member("Milo", "", None).expect("member");
        assert_eq!(milo.name, "Milo");
        assert_eq!(milo.handle, None);
        assert_eq!(milo.mention(), "@Milo");

        // A bot with no name shows its username.
        let bot = member("", "", Some("quiz_bot")).expect("member");
        assert_eq!(bot.name, "quiz_bot");
        // An empty username is no username.
        assert_eq!(member("Nora", "", Some(" ")).expect("member").handle, None);
        assert_eq!(member("", " ", None), None);
    }

    #[test]
    fn a_name_that_two_people_have_is_not_in_the_list() {
        let person = |name: &str, handle: Option<&str>| member(name, "", handle).expect("member");
        let people = vec![
            (person("Sam", None), 1),
            (person("Ada", Some("ada")), 2),
            (person("Sam", None), 3),
            // The name of this person is the handle of another person.
            (person("ada", None), 4),
            (person("Milo", None), 5),
            // Two people with a username can have the same name.
            (person("Ada", Some("ada_2")), 6),
        ];
        let kept: Vec<i64> = clear_mentions(people)
            .into_iter()
            .map(|(_, user_id)| user_id)
            .collect();
        assert_eq!(kept, [2, 5, 6]);
    }

    fn book() -> MemberBook {
        let mut book = MemberBook::default();
        book.set(
            7,
            vec![
                ("@Ada".into(), 1),
                ("@Ada Lovelace".into(), 2),
                ("@Zoë".into(), 3),
            ],
        );
        book
    }

    #[test]
    fn a_name_in_the_text_gets_the_user_id() {
        let found = book().mentions(7, "@Ada Lovelace and @Ada, hello");
        assert_eq!(
            found,
            vec![
                TextMention {
                    offset: 0,
                    length: 13,
                    user_id: 2
                },
                TextMention {
                    offset: 18,
                    length: 4,
                    user_id: 1
                },
            ]
        );
    }

    #[test]
    fn offsets_count_utf16_units() {
        // The emoji is two UTF-16 units and four bytes.
        let found = book().mentions(7, "😀 @Zoë ok");
        assert_eq!(
            found,
            vec![TextMention {
                offset: 3,
                length: 4,
                user_id: 3
            }]
        );
    }

    #[test]
    fn a_name_inside_a_word_is_no_mention() {
        let book = book();
        assert_eq!(book.mentions(7, "mail@Ada"), Vec::new());
        assert_eq!(book.mentions(7, "@Adam"), Vec::new());
        assert_eq!(book.mentions(7, "@Ada_1"), Vec::new());
        assert_eq!(book.mentions(7, "no mention"), Vec::new());
        // Another chat has no list.
        assert_eq!(book.mentions(8, "@Ada"), Vec::new());
    }

    #[test]
    fn the_first_of_two_equal_names_gets_the_mention() {
        let mut book = MemberBook::default();
        book.set(7, vec![("@Sam".into(), 1), ("@Sam".into(), 2)]);
        let found = book.mentions(7, "@Sam");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].user_id, 1);
    }

    #[test]
    fn a_new_list_takes_the_place_of_the_old_one() {
        let mut book = book();
        book.set(7, vec![("@Milo".into(), 9)]);
        assert_eq!(book.mentions(7, "@Ada"), Vec::new());
        assert_eq!(book.mentions(7, "@Milo").len(), 1);
    }
}
