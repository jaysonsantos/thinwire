//! Typing indicators (#510). The core tells the open chat that this account
//! types, and keeps who types in each chat.
//!
//! Out: a change of the draft sends "typing" at most one time each
//! [`SEND_EVERY`]. A draft that becomes empty sends "stop". A sent message
//! ends the typing state on the other side by itself, so a send sends no
//! stop.
//!
//! In: a person shows as typing for [`SHOW_FOR`] after the last event.

use std::collections::HashMap;
use std::fmt;
use std::time::{Duration, Instant};

use thinwire_protocol::{MessageTopic, ProtocolId};

/// The shortest time between two "typing" states for the same thread.
pub(crate) const SEND_EVERY: Duration = Duration::from_secs(5);

/// How long a person shows as typing after the last event.
pub(crate) const SHOW_FOR: Duration = Duration::from_secs(6);

/// A chat, or one topic of a chat.
pub(crate) type Thread = (ProtocolId, String, Option<MessageTopic>);

/// One person who types, and the time of the last event.
struct Typist {
    sender_id: String,
    name: String,
    at: Instant,
}

/// The typing state of this account and of the other people.
#[derive(Default)]
pub(crate) struct Typing {
    /// The thread where this account types, and when the last "typing"
    /// state went out.
    sent: Option<(Thread, Instant)>,
    /// The people who type, by thread, in the order they started.
    people: HashMap<Thread, Vec<Typist>>,
}

/// Names are user data: only the counts show.
impl fmt::Debug for Typing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Typing")
            .field("sending", &self.sent.is_some())
            .field("threads", &self.people.len())
            .finish()
    }
}

impl Typing {
    /// The draft of `thread` changed. The state to send: `Some(true)` for
    /// typing, `Some(false)` for stop, `None` for nothing.
    pub(crate) fn draft_changed(
        &mut self,
        thread: Thread,
        empty: bool,
        now: Instant,
    ) -> Option<bool> {
        let same = self.sent.as_ref().is_some_and(|(sent, _)| *sent == thread);
        if empty {
            return same.then(|| {
                self.sent = None;
                false
            });
        }
        let recent = self
            .sent
            .as_ref()
            .is_some_and(|(_, at)| now.saturating_duration_since(*at) < SEND_EVERY);
        if same && recent {
            return None;
        }
        self.sent = Some((thread, now));
        Some(true)
    }

    /// A message of `thread` went out. It ends the typing state, so the next
    /// change of the draft sends "typing" again.
    pub(crate) fn message_sent(&mut self, thread: &Thread) {
        if self.sent.as_ref().is_some_and(|(sent, _)| sent == thread) {
            self.sent = None;
        }
    }

    /// End the typing state of this account. The thread that still shows it
    /// gets a stop. An older state ended on its own.
    pub(crate) fn stop(&mut self, now: Instant) -> Option<Thread> {
        let (thread, at) = self.sent.take()?;
        (now.saturating_duration_since(at) < SHOW_FOR).then_some(thread)
    }

    /// A person of `thread` started or stopped typing. `true` when the
    /// names of that thread changed.
    pub(crate) fn note(
        &mut self,
        thread: Thread,
        sender_id: String,
        name: String,
        typing: bool,
        now: Instant,
    ) -> bool {
        let people = self.people.entry(thread.clone()).or_default();
        let found = people
            .iter()
            .position(|typist| typist.sender_id == sender_id);
        let changed = match (typing, found) {
            (true, Some(index)) => {
                let typist = &mut people[index];
                typist.at = now;
                let renamed = typist.name != name;
                typist.name = name;
                renamed
            }
            (true, None) => {
                people.push(Typist {
                    sender_id,
                    name,
                    at: now,
                });
                true
            }
            (false, Some(index)) => {
                people.remove(index);
                true
            }
            (false, None) => false,
        };
        if people.is_empty() {
            self.people.remove(&thread);
        }
        changed
    }

    /// A message of the person `name` came in `thread`: the message ends
    /// the typing state of that person. A message has the name of its
    /// sender only, so the name finds the person.
    pub(crate) fn message_from(&mut self, thread: &Thread, name: &str) {
        if let Some(people) = self.people.get_mut(thread) {
            people.retain(|typist| typist.name != name);
            if people.is_empty() {
                self.people.remove(thread);
            }
        }
    }

    /// Drop each person with no event for [`SHOW_FOR`]. `true` when a name
    /// went away.
    pub(crate) fn expire(&mut self, now: Instant) -> bool {
        let mut changed = false;
        self.people.retain(|_, people| {
            let before = people.len();
            people.retain(|typist| now.saturating_duration_since(typist.at) < SHOW_FOR);
            changed |= people.len() != before;
            !people.is_empty()
        });
        changed
    }

    /// The names of the people who type in `thread`, in the order they
    /// started.
    pub(crate) fn names(&self, thread: &Thread) -> Vec<&str> {
        self.people.get(thread).map_or_else(Vec::new, |people| {
            people.iter().map(|typist| typist.name.as_str()).collect()
        })
    }

    /// The session of `protocol` ended: forget its threads.
    pub(crate) fn forget(&mut self, protocol: ProtocolId) {
        self.people.retain(|(owner, _, _), _| *owner != protocol);
        if self
            .sent
            .as_ref()
            .is_some_and(|((owner, _, _), _)| *owner == protocol)
        {
            self.sent = None;
        }
    }
}

/// The line over the compose field: "Ada is typing…", "Ada and Bob are
/// typing…", "Ada, Bob, and Cy are typing…", "Ada, Bob, and 3 others are
/// typing…". `None` when nobody types.
#[must_use]
pub fn typing_line(names: &[&str]) -> Option<String> {
    match names {
        [] => None,
        [one] => Some(format!("{one} is typing…")),
        [one, two] => Some(format!("{one} and {two} are typing…")),
        [one, two, three] => Some(format!("{one}, {two}, and {three} are typing…")),
        [one, two, rest @ ..] => Some(format!(
            "{one}, {two}, and {} others are typing…",
            rest.len()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread(chat: &str) -> Thread {
        (ProtocolId::Telegram, chat.to_owned(), None)
    }

    #[test]
    fn typing_goes_out_at_most_one_time_each_five_seconds() {
        let start = Instant::now();
        let mut typing = Typing::default();
        assert_eq!(typing.draft_changed(thread("a"), false, start), Some(true));
        let soon = start + Duration::from_secs(4);
        assert_eq!(typing.draft_changed(thread("a"), false, soon), None);
        let later = start + SEND_EVERY;
        assert_eq!(typing.draft_changed(thread("a"), false, later), Some(true));
    }

    #[test]
    fn another_thread_sends_typing_at_once() {
        let start = Instant::now();
        let mut typing = Typing::default();
        assert_eq!(typing.draft_changed(thread("a"), false, start), Some(true));
        assert_eq!(typing.draft_changed(thread("b"), false, start), Some(true));
    }

    #[test]
    fn an_empty_draft_stops_only_a_thread_that_typed() {
        let start = Instant::now();
        let mut typing = Typing::default();
        assert_eq!(typing.draft_changed(thread("a"), true, start), None);
        assert_eq!(typing.draft_changed(thread("a"), false, start), Some(true));
        assert_eq!(typing.draft_changed(thread("b"), true, start), None);
        assert_eq!(typing.draft_changed(thread("a"), true, start), Some(false));
        assert_eq!(typing.draft_changed(thread("a"), true, start), None);
    }

    #[test]
    fn a_sent_message_lets_the_next_change_send_typing_again() {
        let start = Instant::now();
        let mut typing = Typing::default();
        assert_eq!(typing.draft_changed(thread("a"), false, start), Some(true));
        typing.message_sent(&thread("a"));
        assert_eq!(typing.draft_changed(thread("a"), true, start), None);
        assert_eq!(typing.draft_changed(thread("a"), false, start), Some(true));
    }

    #[test]
    fn a_stop_names_only_a_state_that_still_shows() {
        let start = Instant::now();
        let mut typing = Typing::default();
        assert_eq!(typing.stop(start), None);
        let _ = typing.draft_changed(thread("a"), false, start);
        assert_eq!(typing.stop(start), Some(thread("a")));
        assert_eq!(typing.stop(start), None);
        let _ = typing.draft_changed(thread("a"), false, start);
        assert_eq!(typing.stop(start + SHOW_FOR), None);
    }

    #[test]
    fn a_person_types_until_a_stop_or_six_quiet_seconds() {
        let start = Instant::now();
        let mut typing = Typing::default();
        assert!(typing.note(thread("a"), "u1".into(), "Ada".into(), true, start));
        assert!(typing.note(thread("a"), "u2".into(), "Bob".into(), true, start));
        assert_eq!(typing.names(&thread("a")), ["Ada", "Bob"]);
        assert!(typing.names(&thread("b")).is_empty());
        // A repeat event keeps the person and changes no name.
        let later = start + Duration::from_secs(4);
        assert!(!typing.note(thread("a"), "u1".into(), "Ada".into(), true, later));
        assert!(typing.note(thread("a"), "u2".into(), "Bob".into(), false, later));
        assert_eq!(typing.names(&thread("a")), ["Ada"]);
        assert!(!typing.expire(start + SHOW_FOR));
        assert!(typing.expire(later + SHOW_FOR));
        assert!(typing.names(&thread("a")).is_empty());
        assert!(!typing.note(thread("a"), "u1".into(), "Ada".into(), false, later));
    }

    #[test]
    fn a_message_of_a_person_ends_their_typing() {
        let start = Instant::now();
        let mut typing = Typing::default();
        let _ = typing.note(thread("a"), "u1".into(), "Ada".into(), true, start);
        let _ = typing.note(thread("a"), "u2".into(), "Bob".into(), true, start);
        let _ = typing.note(thread("b"), "u1".into(), "Ada".into(), true, start);
        typing.message_from(&thread("a"), "Ada");
        assert_eq!(typing.names(&thread("a")), ["Bob"]);
        assert_eq!(typing.names(&thread("b")), ["Ada"], "another chat");
        typing.message_from(&thread("a"), "Bob");
        assert!(typing.names(&thread("a")).is_empty());
        typing.message_from(&thread("c"), "Bob");
    }

    #[test]
    fn an_ended_session_forgets_its_typing() {
        let start = Instant::now();
        let mut typing = Typing::default();
        let _ = typing.note(thread("a"), "u1".into(), "Ada".into(), true, start);
        let _ = typing.draft_changed(thread("a"), false, start);
        let other = (ProtocolId::Discord, "d".to_owned(), None);
        let _ = typing.note(other.clone(), "u9".into(), "Zed".into(), true, start);
        typing.forget(ProtocolId::Telegram);
        assert!(typing.names(&thread("a")).is_empty());
        assert_eq!(typing.names(&other), ["Zed"]);
        assert_eq!(typing.stop(start), None);
    }

    #[test]
    fn the_line_names_up_to_three_people() {
        assert_eq!(typing_line(&[]), None);
        assert_eq!(typing_line(&["Ada"]).as_deref(), Some("Ada is typing…"));
        assert_eq!(
            typing_line(&["Ada", "Bob"]).as_deref(),
            Some("Ada and Bob are typing…")
        );
        assert_eq!(
            typing_line(&["Ada", "Bob", "Cy"]).as_deref(),
            Some("Ada, Bob, and Cy are typing…")
        );
        assert_eq!(
            typing_line(&["Ada", "Bob", "Cy", "Di", "Eve"]).as_deref(),
            Some("Ada, Bob, and 3 others are typing…")
        );
    }

    #[test]
    fn debug_shows_no_name() {
        let mut typing = Typing::default();
        let _ = typing.note(thread("a"), "u1".into(), "Ada".into(), true, Instant::now());
        let text = format!("{typing:?}");
        assert!(!text.contains("Ada") && !text.contains("u1"), "{text}");
    }
}
