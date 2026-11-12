//! Older pages of each thread (#30). The core asks an adapter for the page
//! before the oldest loaded message, and this module keeps what each thread
//! knows about those requests:
//!
//! - a request is in flight (one at a time for each thread);
//! - the start of the thread is loaded, so no request goes out;
//! - the last request brought nothing older, so its anchor waits (#61, #67);
//! - the last page did not load (#57);
//! - the runs of stored rows that the thread could not read;
//! - the store cursor of the next read, and the cursor last sent, so a
//!   cursor that does not move cannot loop.
//!
//! Every fact of one thread is in one value. A chat or an account that goes
//! away drops it in one place.

use std::collections::HashMap;
use std::fmt;
use std::time::Instant;

use thinwire_protocol::{HistoryStoreCursor, ProtocolId};

use crate::state::{UnreadableRun, older_wait};
use crate::typing::Thread;

/// Skips not drawn yet. Empty pages add to it until a page shows rows or
/// the store ends.
struct OpenRun {
    before_id: Option<String>,
    skipped: u32,
}

/// An older page that brought nothing new: the anchor, when, and how many
/// times.
struct Retry {
    anchor: String,
    at: Instant,
    tries: u32,
}

#[derive(Default)]
struct ThreadPaging {
    /// A request for older messages is in flight.
    loading: bool,
    /// The start of the thread is loaded. No more older requests.
    at_start: bool,
    /// The last older request brought nothing older.
    retry: Option<Retry>,
    /// The last older page did not load. A later success clears it.
    failed: bool,
    /// Closed runs of unreadable stored rows, oldest request first.
    runs: Vec<UnreadableRun>,
    /// A run that is still loading. Empty pages add to it.
    open: Option<OpenRun>,
    /// The next older read of this thread starts at this store timestamp.
    resume: Option<u64>,
    /// The store timestamp last sent for this thread. The next cursor must
    /// be older, so a stuck cursor cannot loop.
    asked: Option<u64>,
}

/// One page that came with a store cursor.
pub(crate) struct StorePage<'a> {
    /// The anchor of the request. Empty for a request with no anchor.
    pub(crate) request_before: &'a str,
    pub(crate) cursor: HistoryStoreCursor,
    pub(crate) more: bool,
    /// The first page of the thread, not an older page.
    pub(crate) initial: bool,
    /// The oldest loaded message of the thread, after this page.
    pub(crate) oldest: Option<String>,
}

/// The older-page state of every thread.
#[derive(Default)]
pub(crate) struct Paging {
    threads: HashMap<Thread, ThreadPaging>,
}

/// A thread key has a chat id: only the count shows.
impl fmt::Debug for Paging {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Paging")
            .field("threads", &self.threads.len())
            .field("loading", &self.any_loading())
            .finish()
    }
}

fn push_run(runs: &mut Vec<UnreadableRun>, before_id: Option<String>, skipped: u32) {
    if skipped == 0 {
        return;
    }
    if let Some(run) = runs.iter_mut().find(|run| run.before_id == before_id) {
        run.skipped = run.skipped.saturating_add(skipped);
        return;
    }
    runs.push(UnreadableRun { before_id, skipped });
}

fn run_anchor(initial: bool, request_before: &str, oldest: Option<String>) -> Option<String> {
    if initial {
        oldest
    } else if request_before.is_empty() {
        None
    } else {
        Some(request_before.to_string())
    }
}

impl Paging {
    fn thread(&mut self, key: &Thread) -> &mut ThreadPaging {
        self.threads.entry(key.clone()).or_default()
    }

    fn has(&self, key: &Thread, fact: impl Fn(&ThreadPaging) -> bool) -> bool {
        self.threads.get(key).is_some_and(fact)
    }

    /// A request for older messages of this thread is in flight.
    pub(crate) fn is_loading(&self, key: &Thread) -> bool {
        self.has(key, |thread| thread.loading)
    }

    /// A request for older messages of some thread is in flight.
    pub(crate) fn any_loading(&self) -> bool {
        self.threads.values().any(|thread| thread.loading)
    }

    /// A request for older messages of this account is in flight.
    pub(crate) fn protocol_loading(&self, protocol: ProtocolId) -> bool {
        self.threads
            .iter()
            .any(|(key, thread)| key.0 == protocol && thread.loading)
    }

    /// The start of this thread is loaded.
    pub(crate) fn at_start(&self, key: &Thread) -> bool {
        self.has(key, |thread| thread.at_start)
    }

    /// The last older page of this thread did not load.
    pub(crate) fn failed(&self, key: &Thread) -> bool {
        self.has(key, |thread| thread.failed)
    }

    /// The closed runs of unreadable rows of this thread.
    pub(crate) fn runs(&self, key: &Thread) -> &[UnreadableRun] {
        self.threads
            .get(key)
            .map(|thread| thread.runs.as_slice())
            .unwrap_or(&[])
    }

    /// A request with the anchor `oldest` can go out at `now`: no request
    /// is in flight, the start is not loaded, and the anchor does not wait.
    pub(crate) fn may_ask(&self, key: &Thread, oldest: &str, now: Instant) -> bool {
        let Some(thread) = self.threads.get(key) else {
            return true;
        };
        if thread.loading || thread.at_start {
            return false;
        }
        !thread.retry.as_ref().is_some_and(|retry| {
            retry.anchor == oldest
                && now.saturating_duration_since(retry.at) < older_wait(retry.tries)
        })
    }

    /// A request for older messages of this thread goes out.
    pub(crate) fn asked(&mut self, key: &Thread) {
        self.thread(key).loading = true;
    }

    /// The saved store cursor, when it is older than the last finished
    /// request. A failed read clears that timestamp, so the same cursor is
    /// sent again. A cursor that a finished read did not move is dropped.
    /// A topic has no store cursor.
    pub(crate) fn take_resume_cursor(&mut self, key: &Thread) -> Option<u64> {
        if key.2.is_some() {
            return None;
        }
        let thread = self.threads.get_mut(key)?;
        let cursor = thread.resume?;
        if thread.asked.is_some_and(|asked| cursor >= asked) {
            thread.resume = None;
            return None;
        }
        thread.asked = Some(cursor);
        Some(cursor)
    }

    /// The answer of an older request arrived. A failure sets the note of
    /// the thread, and a success clears it.
    ///
    /// A failed page keeps the open run and the store cursor, so the retry
    /// sends the same read. The asked timestamp is the in-flight cursor: if
    /// it stayed, that retry would look like a cursor that did not move.
    pub(crate) fn answered(&mut self, key: &Thread, failed: bool) {
        let thread = self.thread(key);
        thread.failed = failed;
        thread.loading = false;
        if failed {
            thread.asked = None;
        }
    }

    /// A page that loaded came with no store cursor: the open run closes,
    /// and the next read has no cursor.
    pub(crate) fn no_store_cursor(&mut self, key: &Thread) {
        let thread = self.thread(key);
        if let Some(open) = thread.open.take()
            && open.skipped > 0
        {
            thread.runs.push(UnreadableRun {
                before_id: open.before_id,
                skipped: open.skipped,
            });
        }
        thread.resume = None;
    }

    /// The end of one older request with the anchor `before`. `oldest` is
    /// the oldest loaded message now. With no more pages, the start is
    /// loaded. A page that brought nothing older makes its anchor wait, and
    /// a repeat on the same anchor waits longer (#67).
    pub(crate) fn settle(
        &mut self,
        key: &Thread,
        more: bool,
        before: String,
        oldest: Option<&str>,
        now: Instant,
    ) {
        let thread = self.thread(key);
        if !more {
            thread.retry = None;
            thread.at_start = true;
        } else if oldest == Some(before.as_str()) {
            let tries = match &thread.retry {
                Some(retry) if retry.anchor == before => retry.tries + 1,
                _ => 1,
            };
            thread.retry = Some(Retry {
                anchor: before,
                at: now,
                tries,
            });
        } else {
            thread.retry = None;
        }
    }

    /// Record the store cursor of one page. `is_loaded` says if the thread
    /// has a message id. The answer is `Some` when this page held no rows
    /// and the next page must go out at once: the value is the store
    /// timestamp of that request, and the thread is loading again.
    pub(crate) fn store_page(
        &mut self,
        key: &Thread,
        page: StorePage<'_>,
        is_loaded: impl Fn(&str) -> bool,
    ) -> Option<u64> {
        let StorePage {
            request_before,
            cursor,
            more,
            initial,
            oldest,
        } = page;
        let no_topic = key.2.is_none();
        let added = if initial {
            oldest.is_some()
        } else {
            oldest.as_deref().is_some_and(|id| id != request_before)
        };
        let thread = self.thread(key);
        let moves = thread.asked.is_none_or(|asked| cursor.before_ms < asked);
        if !added && cursor.resume && moves && more && no_topic {
            let before_id = thread.open.as_ref().map_or_else(
                || {
                    if initial || request_before.is_empty() {
                        None
                    } else {
                        Some(request_before.to_string())
                    }
                },
                |open| open.before_id.clone(),
            );
            let skipped = thread
                .open
                .as_ref()
                .map(|open| open.skipped)
                .unwrap_or(0)
                .saturating_add(cursor.skipped);
            thread.open = Some(OpenRun { before_id, skipped });
            return thread.queue(cursor.before_ms).then_some(cursor.before_ms);
        }
        let pending = thread.open.take();
        if cursor.runs.is_empty() {
            let skipped = pending
                .as_ref()
                .map(|open| open.skipped)
                .unwrap_or(0)
                .saturating_add(cursor.skipped);
            if skipped > 0 {
                let before_id = pending.map_or_else(
                    || run_anchor(initial, request_before, oldest),
                    |open| open.before_id,
                );
                thread.runs.push(UnreadableRun { before_id, skipped });
            }
        } else {
            let mut leading = pending.as_ref().map(|open| open.skipped).unwrap_or(0);
            let pending_anchor = pending.map(|open| open.before_id);
            let mut edge = 0u32;
            let mut placed = Vec::new();
            for run in cursor.runs {
                if run.skipped == 0 {
                    continue;
                }
                match run.after_id {
                    None => leading = leading.saturating_add(run.skipped),
                    Some(id) if is_loaded(&id) => {
                        push_run(&mut placed, Some(id), run.skipped);
                    }
                    Some(_) => {
                        tracing::warn!(
                            "history run names a message that is not loaded; the count joins the oldest line"
                        );
                        edge = edge.saturating_add(run.skipped);
                    }
                }
            }
            if edge > 0 {
                push_run(&mut placed, oldest.clone(), edge);
            }
            if leading > 0 {
                let before_id =
                    pending_anchor.unwrap_or_else(|| run_anchor(initial, request_before, oldest));
                push_run(&mut placed, before_id, leading);
            }
            thread.runs.extend(placed);
        }
        thread.resume =
            (cursor.resume && moves && more && added && no_topic).then_some(cursor.before_ms);
        None
    }

    /// Drop what is known about one chat, for every topic.
    pub(crate) fn forget_chat(&mut self, protocol: ProtocolId, chat: &str) {
        self.threads
            .retain(|key, _| !(key.0 == protocol && key.1 == chat));
    }

    /// Drop what is known about one account.
    pub(crate) fn forget_protocol(&mut self, protocol: ProtocolId) {
        self.threads.retain(|key, _| key.0 != protocol);
    }

    /// End the requests in flight of one account: of one chat, or of all
    /// its chats. An answer can be lost in a reconnect. A later scroll asks
    /// again.
    pub(crate) fn stop_loading(&mut self, protocol: ProtocolId, chat: Option<&str>) {
        for (key, thread) in &mut self.threads {
            if key.0 == protocol && chat.is_none_or(|chat| key.1 == chat) {
                thread.loading = false;
            }
        }
    }
}

impl ThreadPaging {
    /// The next page from `before_ms` goes out now. `false` when that
    /// timestamp does not move older than the last request.
    fn queue(&mut self, before_ms: u64) -> bool {
        if self.asked.is_some_and(|asked| before_ms >= asked) {
            self.resume = None;
            return false;
        }
        self.resume = Some(before_ms);
        self.asked = Some(before_ms);
        self.loading = true;
        self.retry = None;
        self.at_start = false;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::OLDER_RETRY_DELAY;
    use thinwire_protocol::HistorySkipRun;

    fn key() -> Thread {
        (ProtocolId::Telegram, "chat".into(), None)
    }

    fn cursor(before_ms: u64, skipped: u32) -> HistoryStoreCursor {
        HistoryStoreCursor {
            before_ms,
            skipped,
            resume: true,
            runs: Vec::new(),
        }
    }

    fn page(cursor: HistoryStoreCursor, oldest: Option<&str>) -> StorePage<'static> {
        StorePage {
            request_before: "m5",
            cursor,
            more: true,
            initial: false,
            oldest: oldest.map(str::to_owned),
        }
    }

    #[test]
    fn one_request_at_a_time_for_each_thread() {
        let mut paging = Paging::default();
        let now = Instant::now();
        assert!(paging.may_ask(&key(), "m5", now));
        paging.asked(&key());
        assert!(paging.is_loading(&key()));
        assert!(paging.protocol_loading(ProtocolId::Telegram));
        assert!(!paging.may_ask(&key(), "m5", now));
        let topic: Thread = (ProtocolId::Telegram, "other".into(), None);
        assert!(paging.may_ask(&topic, "m5", now), "another thread asks");
        paging.answered(&key(), false);
        assert!(!paging.any_loading());
    }

    #[test]
    fn the_start_of_a_thread_ends_its_requests() {
        let mut paging = Paging::default();
        let now = Instant::now();
        paging.asked(&key());
        paging.answered(&key(), false);
        paging.settle(&key(), false, "m5".into(), Some("m1"), now);
        assert!(paging.at_start(&key()));
        assert!(!paging.may_ask(&key(), "m1", now));
    }

    #[test]
    fn an_anchor_that_brought_nothing_waits_longer_each_time() {
        let mut paging = Paging::default();
        let start = Instant::now();
        paging.settle(&key(), true, "m5".into(), Some("m5"), start);
        assert!(!paging.may_ask(&key(), "m5", start + OLDER_RETRY_DELAY / 2));
        assert!(paging.may_ask(&key(), "m5", start + OLDER_RETRY_DELAY));
        assert!(
            paging.may_ask(&key(), "m4", start),
            "a new anchor asks at once"
        );
        paging.settle(&key(), true, "m5".into(), Some("m5"), start);
        assert!(!paging.may_ask(&key(), "m5", start + OLDER_RETRY_DELAY));
        assert!(paging.may_ask(&key(), "m5", start + OLDER_RETRY_DELAY * 2));
        paging.settle(&key(), true, "m5".into(), Some("m3"), start);
        assert!(
            paging.may_ask(&key(), "m3", start),
            "older rows end the wait"
        );
    }

    #[test]
    fn a_failed_page_sets_the_note_and_a_success_clears_it() {
        let mut paging = Paging::default();
        paging.asked(&key());
        paging.answered(&key(), true);
        assert!(paging.failed(&key()));
        assert!(!paging.is_loading(&key()));
        paging.answered(&key(), false);
        assert!(!paging.failed(&key()));
    }

    #[test]
    fn empty_pages_join_into_one_run_and_ask_the_next_page() {
        let mut paging = Paging::default();
        // Two pages with no rows: the oldest message stays the anchor.
        let next = paging.store_page(&key(), page(cursor(900, 2), Some("m5")), |_| true);
        assert_eq!(next, Some(900));
        assert!(paging.is_loading(&key()));
        assert!(paging.runs(&key()).is_empty(), "the run is still open");
        let next = paging.store_page(&key(), page(cursor(800, 3), Some("m5")), |_| true);
        assert_eq!(next, Some(800));
        // A page with rows closes the run with the sum.
        let next = paging.store_page(&key(), page(cursor(700, 0), Some("m2")), |_| true);
        assert_eq!(next, None);
        assert_eq!(
            paging.runs(&key()),
            [UnreadableRun {
                before_id: Some("m5".into()),
                skipped: 5
            }]
        );
        assert_eq!(paging.take_resume_cursor(&key()), Some(700));
    }

    #[test]
    fn a_cursor_that_does_not_move_does_not_loop() {
        let mut paging = Paging::default();
        assert_eq!(
            paging.store_page(&key(), page(cursor(900, 1), Some("m5")), |_| true),
            Some(900)
        );
        // The store answers with the same timestamp: no new request.
        assert_eq!(
            paging.store_page(&key(), page(cursor(900, 1), Some("m5")), |_| true),
            None
        );
        assert_eq!(paging.take_resume_cursor(&key()), None);
    }

    #[test]
    fn a_failed_read_sends_the_same_cursor_again() {
        let mut paging = Paging::default();
        paging.store_page(&key(), page(cursor(700, 0), Some("m2")), |_| true);
        assert_eq!(paging.take_resume_cursor(&key()), Some(700));
        paging.asked(&key());
        paging.answered(&key(), true);
        assert_eq!(paging.take_resume_cursor(&key()), Some(700));
    }

    #[test]
    fn a_run_goes_before_the_loaded_message_it_names() {
        let mut paging = Paging::default();
        let mut with_runs = cursor(700, 0);
        with_runs.runs = vec![
            HistorySkipRun {
                after_id: Some("m3".into()),
                skipped: 2,
            },
            HistorySkipRun {
                after_id: Some("gone".into()),
                skipped: 1,
            },
        ];
        paging.store_page(&key(), page(with_runs, Some("m2")), |id| id == "m3");
        assert_eq!(
            paging.runs(&key()),
            [
                UnreadableRun {
                    before_id: Some("m3".into()),
                    skipped: 2
                },
                UnreadableRun {
                    before_id: Some("m2".into()),
                    skipped: 1
                },
            ]
        );
    }

    #[test]
    fn a_chat_that_goes_away_drops_every_fact_of_its_threads() {
        let mut paging = Paging::default();
        let topic: Thread = (
            ProtocolId::Telegram,
            "chat".into(),
            Some(thinwire_protocol::MessageTopic::Forum(7)),
        );
        let other: Thread = (ProtocolId::Telegram, "other".into(), None);
        for thread in [key(), topic.clone(), other.clone()] {
            paging.asked(&thread);
            paging.answered(&thread, true);
            paging.settle(&thread, false, "m5".into(), None, Instant::now());
        }
        paging.forget_chat(ProtocolId::Telegram, "chat");
        assert!(!paging.failed(&key()) && !paging.at_start(&key()));
        assert!(!paging.failed(&topic) && !paging.at_start(&topic));
        assert!(paging.failed(&other) && paging.at_start(&other));
        paging.forget_protocol(ProtocolId::Telegram);
        assert!(!paging.failed(&other));
    }

    #[test]
    fn a_reconnect_ends_the_requests_in_flight_only() {
        let mut paging = Paging::default();
        let other: Thread = (ProtocolId::Telegram, "other".into(), None);
        paging.asked(&key());
        paging.asked(&other);
        paging.settle(&other, false, "m5".into(), None, Instant::now());
        paging.stop_loading(ProtocolId::Telegram, Some("chat"));
        assert!(!paging.is_loading(&key()));
        assert!(paging.is_loading(&other));
        paging.stop_loading(ProtocolId::Telegram, None);
        assert!(!paging.any_loading());
        assert!(paging.at_start(&other), "the other facts stay");
    }
}
