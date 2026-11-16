//! The search of message text for one chat list (#514).
//!
//! Each pane has one [`MessageSearch`]. Its target is the text of the search
//! field and the accounts that can search. A new target drops the old
//! results and waits [`SEARCH_DELAY`]: a key press is not a request. Then
//! one `SearchMessages` goes to each account. An answer counts only when
//! its request number is one that this search still waits for, so the
//! answer to an older text never shows.
//!
//! The query, the texts, the names, and the ids are user data: no type here
//! has a `Debug` that shows them.

use std::fmt;
use std::time::{Duration, Instant};

use thinwire_protocol::{FoundMessage, ProtocolId};

/// The time with no change of the text before a request goes out.
pub(crate) const SEARCH_DELAY: Duration = Duration::from_millis(300);

/// The fewest characters of a text that starts a search.
pub const SEARCH_MIN_CHARS: usize = 2;

/// What one pane searches: the text with no space at its ends, and the
/// accounts, in sidebar order.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct SearchTarget {
    pub(crate) query: String,
    pub(crate) accounts: Vec<ProtocolId>,
}

/// The message search of one pane.
#[derive(Clone, Default, PartialEq, Eq)]
pub(crate) struct MessageSearch {
    target: Option<SearchTarget>,
    /// When the requests of `target` go out. `None` after they went out.
    due: Option<Instant>,
    /// The requests with no answer yet.
    waiting: Vec<(ProtocolId, u64)>,
    /// The found messages of every answer, newest first.
    results: Vec<(ProtocolId, FoundMessage)>,
}

impl fmt::Debug for MessageSearch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MessageSearch")
            .field("active", &self.target.is_some())
            .field("waiting", &self.waiting.len())
            .field("results", &self.results.len())
            .finish()
    }
}

impl MessageSearch {
    /// Follow the search field. A new target drops the results and the
    /// open requests, and starts the wait. `true` when the target changed.
    pub(crate) fn retarget(&mut self, target: Option<SearchTarget>, now: Instant) -> bool {
        if self.target == target {
            return false;
        }
        self.due = target.as_ref().map(|_| now + SEARCH_DELAY);
        self.target = target;
        self.waiting.clear();
        self.results.clear();
        true
    }

    /// The session of `account` ended: a request to it gets no answer.
    /// The search stops, and the next [`Self::retarget`] starts it again
    /// when an account is left.
    pub(crate) fn forget_account(&mut self, account: ProtocolId) {
        let uses = self
            .target
            .as_ref()
            .is_some_and(|target| target.accounts.contains(&account));
        if uses {
            *self = Self::default();
        }
    }

    /// When the requests go out.
    pub(crate) fn due(&self) -> Option<Instant> {
        self.due
    }

    /// The target, one time, when its wait ended at `now`.
    pub(crate) fn take_due(&mut self, now: Instant) -> Option<SearchTarget> {
        if self.due.is_some_and(|due| due <= now) {
            self.due = None;
            return self.target.clone();
        }
        None
    }

    /// A request with this number went to `account`.
    pub(crate) fn asked(&mut self, account: ProtocolId, request: u64) {
        self.waiting.push((account, request));
    }

    /// This search waits for the answer to this request.
    pub(crate) fn waits_for(&self, account: ProtocolId, request: u64) -> bool {
        self.waiting.contains(&(account, request))
    }

    /// The answer to one request. `false` when this search does not wait
    /// for it: it is the answer to an older text, or to the other pane.
    pub(crate) fn answer(
        &mut self,
        account: ProtocolId,
        request: u64,
        found: Vec<FoundMessage>,
    ) -> bool {
        let Some(at) = self
            .waiting
            .iter()
            .position(|open| *open == (account, request))
        else {
            return false;
        };
        self.waiting.remove(at);
        self.results
            .extend(found.into_iter().map(|row| (account, row)));
        // A stable sort: the rows of one answer keep their order.
        self.results
            .sort_by_key(|(_, row)| std::cmp::Reverse(row.sent_at));
        true
    }

    /// The text that the results belong to.
    pub(crate) fn query(&self) -> Option<&str> {
        self.target.as_ref().map(|target| target.query.as_str())
    }

    /// A request waits for its time or for its answer.
    pub(crate) fn busy(&self) -> bool {
        self.due.is_some() || !self.waiting.is_empty()
    }

    pub(crate) fn results(&self) -> &[(ProtocolId, FoundMessage)] {
        &self.results
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(query: &str) -> Option<SearchTarget> {
        Some(SearchTarget {
            query: query.to_owned(),
            accounts: vec![ProtocolId::Telegram],
        })
    }

    fn found(id: &str, sent_at: i64) -> FoundMessage {
        FoundMessage {
            conversation_id: "telegram:1".into(),
            message_id: id.into(),
            topic: None,
            sender: "Ada".into(),
            outbound: false,
            sent_at,
            text: "tea".into(),
        }
    }

    /// A request goes out only after the text stopped for the delay.
    #[test]
    fn a_request_waits_until_the_text_stops() {
        let start = Instant::now();
        let mut search = MessageSearch::default();
        assert!(search.retarget(target("te"), start));
        assert!(search.busy());
        assert!(search.take_due(start + SEARCH_DELAY / 2).is_none());
        // One more character starts the wait again.
        let later = start + SEARCH_DELAY / 2;
        assert!(search.retarget(target("tea"), later));
        assert!(search.take_due(start + SEARCH_DELAY).is_none());
        let due = search.take_due(later + SEARCH_DELAY).expect("due");
        assert_eq!(due.query, "tea");
        assert!(search.take_due(later + SEARCH_DELAY * 2).is_none(), "once");
        // The same text again asks nothing.
        assert!(!search.retarget(target("tea"), later + SEARCH_DELAY * 3));
        assert!(search.due().is_none());
    }

    /// The answer to an older text does not show, and an answer shows the
    /// newest message first.
    #[test]
    fn an_old_answer_is_dropped_and_results_are_newest_first() {
        let start = Instant::now();
        let tg = ProtocolId::Telegram;
        let mut search = MessageSearch::default();
        search.retarget(target("te"), start);
        search.take_due(start + SEARCH_DELAY);
        search.asked(tg, 1);
        search.retarget(target("tea"), start);
        search.take_due(start + SEARCH_DELAY);
        search.asked(tg, 2);
        assert!(!search.answer(tg, 1, vec![found("old", 5)]));
        assert!(search.results().is_empty());
        assert!(search.busy());
        assert!(!search.answer(ProtocolId::Slack, 2, Vec::new()), "account");
        assert!(search.answer(tg, 2, vec![found("a", 10), found("b", 30)]));
        assert!(!search.busy());
        let ids: Vec<&str> = search
            .results()
            .iter()
            .map(|(_, row)| row.message_id.as_str())
            .collect();
        assert_eq!(ids, ["b", "a"]);
        assert!(!search.answer(tg, 2, vec![found("c", 40)]), "one answer");
        // A cleared text drops the results.
        assert!(search.retarget(None, start));
        assert!(search.results().is_empty());
        assert!(!search.busy());
        assert!(search.query().is_none());
    }

    /// A session end stops the search of its account, so no request waits
    /// for an answer that does not come.
    #[test]
    fn a_session_end_stops_the_search_of_its_account() {
        let start = Instant::now();
        let mut search = MessageSearch::default();
        search.retarget(target("tea"), start);
        search.take_due(start + SEARCH_DELAY);
        search.asked(ProtocolId::Telegram, 1);
        search.forget_account(ProtocolId::Slack);
        assert!(search.busy(), "another account changes nothing");
        search.forget_account(ProtocolId::Telegram);
        assert!(!search.busy());
        assert!(search.query().is_none());
    }

    /// `Debug` shows no text of the user.
    #[test]
    fn debug_shows_no_query_and_no_result() {
        let start = Instant::now();
        let mut search = MessageSearch::default();
        search.retarget(target("query-fixture-91c"), start);
        search.take_due(start + SEARCH_DELAY);
        search.asked(ProtocolId::Telegram, 1);
        search.answer(ProtocolId::Telegram, 1, vec![found("id-fixture-77", 1)]);
        let shown = format!("{search:?}");
        for secret in ["query-fixture-91c", "id-fixture-77", "tea", "Ada"] {
            assert!(!shown.contains(secret), "{shown}");
        }
    }
}
