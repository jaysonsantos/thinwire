//! One tracker for the sends and retries of every protocol.
//!
//! A chat has at most one send or retry in flight. Only the adapter's
//! `SendAccepted` / `SendRejected` for the same request id ends it. A status
//! (`Connecting`, `Error`), a command failure, or a notice never does. The
//! end of a protocol's session drops its entries, and an entry with no answer
//! after [`SEND_TIMEOUT`] expires (#69).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use thinwire_protocol::ProtocolId;

/// What is in flight for one chat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Pending {
    /// New text from the compose field. `body` is the trimmed text, so an
    /// accepted send clears the matching draft only.
    Send { request: u64, body: String },
    /// A failed message sent again.
    Retry { request: u64, message_id: String },
}

impl Pending {
    const fn request(&self) -> u64 {
        match self {
            Self::Send { request, .. } | Self::Retry { request, .. } => *request,
        }
    }
}

/// Longest wait for the adapter's answer to a send or retry. After it, the
/// chat unlocks and the send counts as failed (#69). An adapter answers far
/// sooner: this is a safety net for a lost answer, not the normal path.
pub(crate) const SEND_TIMEOUT: Duration = Duration::from_secs(30);

/// One entry: what is in flight, and since when.
#[derive(Debug)]
struct Open {
    pending: Pending,
    since: Instant,
}

/// Sends and retries in flight, keyed by (protocol, chat).
#[derive(Debug)]
pub(crate) struct SendTracker {
    /// The next request id. Unique for the whole app, never per protocol.
    next: u64,
    open: HashMap<(ProtocolId, String), Open>,
}

impl Default for SendTracker {
    fn default() -> Self {
        Self {
            next: 1,
            open: HashMap::new(),
        }
    }
}

impl SendTracker {
    /// The request id of this chat's send or retry in flight, if any.
    #[cfg(test)]
    pub(crate) fn request_of(&self, protocol: ProtocolId, chat: &str) -> Option<u64> {
        self.open
            .get(&(protocol, chat.to_owned()))
            .map(|open| open.pending.request())
    }

    /// Start a send. `None` while this chat already has a send or a retry.
    pub(crate) fn begin_send(
        &mut self,
        protocol: ProtocolId,
        chat: &str,
        body: &str,
        now: Instant,
    ) -> Option<u64> {
        self.begin(protocol, chat, now, |request| Pending::Send {
            request,
            body: body.to_owned(),
        })
    }

    /// Start a retry. `None` while this chat already has a send or a retry.
    pub(crate) fn begin_retry(
        &mut self,
        protocol: ProtocolId,
        chat: &str,
        message_id: &str,
        now: Instant,
    ) -> Option<u64> {
        self.begin(protocol, chat, now, |request| Pending::Retry {
            request,
            message_id: message_id.to_owned(),
        })
    }

    fn begin(
        &mut self,
        protocol: ProtocolId,
        chat: &str,
        now: Instant,
        pending: impl FnOnce(u64) -> Pending,
    ) -> Option<u64> {
        let key = (protocol, chat.to_owned());
        if self.open.contains_key(&key) {
            return None;
        }
        let request = self.next;
        self.next += 1;
        self.open.insert(
            key,
            Open {
                pending: pending(request),
                since: now,
            },
        );
        Some(request)
    }

    /// No send or retry is in flight in any chat.
    pub(crate) fn is_empty(&self) -> bool {
        self.open.is_empty()
    }

    /// A send or retry of this chat is in flight.
    pub(crate) fn in_flight(&self, protocol: ProtocolId, chat: &str) -> bool {
        self.open.contains_key(&(protocol, chat.to_owned()))
    }

    /// The adapter answered `request` for this chat. Returns and removes the
    /// entry only when the request id matches; an old id changes nothing.
    pub(crate) fn settle(
        &mut self,
        protocol: ProtocolId,
        chat: &str,
        request: u64,
    ) -> Option<Pending> {
        let key = (protocol, chat.to_owned());
        if self.open.get(&key).map(|open| open.pending.request()) != Some(request) {
            return None;
        }
        self.open.remove(&key).map(|open| open.pending)
    }

    /// When the oldest entry expires, if any entry is in flight. The core
    /// arms a wake for it, so a frontend that waits on the change signal
    /// pumps at the deadline (PR #81 review).
    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.open
            .values()
            .map(|open| open.since + SEND_TIMEOUT)
            .min()
    }

    /// Test hook: move every entry's start back by `by`.
    #[cfg(test)]
    pub(crate) fn age_for_test(&mut self, by: Duration) {
        for open in self.open.values_mut() {
            open.since -= by;
        }
    }

    /// Remove and return every entry with no answer after `SEND_TIMEOUT`
    /// (#69). A late answer for one of them then changes nothing.
    pub(crate) fn expire(&mut self, now: Instant) -> Vec<(ProtocolId, String, Pending)> {
        let late: Vec<(ProtocolId, String)> = self
            .open
            .iter()
            .filter(|(_, open)| now.saturating_duration_since(open.since) >= SEND_TIMEOUT)
            .map(|(key, _)| key.clone())
            .collect();
        late.into_iter()
            .filter_map(|key| {
                self.open
                    .remove(&key)
                    .map(|open| (key.0, key.1, open.pending))
            })
            .collect()
    }

    /// The protocol's session ended: its sends and retries are gone.
    pub(crate) fn drop_protocol(&mut self, protocol: ProtocolId) {
        self.open.retain(|(owner, _), _| *owner != protocol);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_send_per_chat_and_the_same_chat_id_in_two_protocols_does_not_block() {
        let mut sends = SendTracker::default();
        let first = sends
            .begin_send(ProtocolId::Slack, "c1", "hi", Instant::now())
            .expect("first");
        assert!(
            sends
                .begin_send(ProtocolId::Slack, "c1", "again", Instant::now())
                .is_none()
        );
        assert!(
            sends
                .begin_retry(ProtocolId::Slack, "c1", "m1", Instant::now())
                .is_none()
        );
        let other = sends
            .begin_send(ProtocolId::Discord, "c1", "hi", Instant::now())
            .expect("another protocol");
        assert_ne!(first, other, "request ids are unique for the app");
        assert!(sends.in_flight(ProtocolId::Slack, "c1"));
        assert!(!sends.in_flight(ProtocolId::Slack, "c2"));
    }

    #[test]
    fn only_the_matching_request_settles() {
        let mut sends = SendTracker::default();
        let request = sends
            .begin_send(ProtocolId::Telegram, "telegram:1", "hi", Instant::now())
            .expect("send");
        assert_eq!(
            sends.settle(ProtocolId::Telegram, "telegram:1", request + 7),
            None
        );
        assert!(sends.in_flight(ProtocolId::Telegram, "telegram:1"));
        assert_eq!(
            sends.settle(ProtocolId::Telegram, "telegram:1", request),
            Some(Pending::Send {
                request,
                body: "hi".into()
            })
        );
        assert!(!sends.in_flight(ProtocolId::Telegram, "telegram:1"));
        assert_eq!(
            sends.settle(ProtocolId::Telegram, "telegram:1", request),
            None
        );
    }

    #[test]
    fn a_retry_is_tracked_like_a_send() {
        let mut sends = SendTracker::default();
        let request = sends
            .begin_retry(ProtocolId::WhatsApp, "wa:1", "wa:1:9", Instant::now())
            .expect("retry");
        assert!(
            sends
                .begin_send(ProtocolId::WhatsApp, "wa:1", "new", Instant::now())
                .is_none()
        );
        assert_eq!(
            sends.settle(ProtocolId::WhatsApp, "wa:1", request),
            Some(Pending::Retry {
                request,
                message_id: "wa:1:9".into()
            })
        );
    }

    #[test]
    fn drop_protocol_clears_only_that_protocol() {
        let mut sends = SendTracker::default();
        sends.begin_send(ProtocolId::Slack, "c1", "a", Instant::now());
        sends.begin_retry(ProtocolId::Slack, "c2", "m", Instant::now());
        sends.begin_send(ProtocolId::Telegram, "telegram:1", "b", Instant::now());
        sends.drop_protocol(ProtocolId::Slack);
        assert!(!sends.in_flight(ProtocolId::Slack, "c1"));
        assert!(!sends.in_flight(ProtocolId::Slack, "c2"));
        assert!(sends.in_flight(ProtocolId::Telegram, "telegram:1"));
    }

    /// #69: an entry with no answer expires after `SEND_TIMEOUT`; a late
    /// answer then changes nothing.
    #[test]
    fn an_unanswered_send_expires() {
        let start = Instant::now();
        let mut sends = SendTracker::default();
        let request = sends
            .begin_send(ProtocolId::Telegram, "telegram:1", "hi", start)
            .expect("send");
        sends
            .begin_retry(ProtocolId::Slack, "c1", "m1", start + SEND_TIMEOUT / 2)
            .expect("retry");
        assert!(sends.expire(start + SEND_TIMEOUT / 2).is_empty(), "not yet");
        let late = sends.expire(start + SEND_TIMEOUT);
        assert_eq!(late.len(), 1, "only the older one: {late:?}");
        assert_eq!(late[0].0, ProtocolId::Telegram);
        assert!(!sends.in_flight(ProtocolId::Telegram, "telegram:1"));
        assert!(sends.in_flight(ProtocolId::Slack, "c1"));
        assert_eq!(
            sends.settle(ProtocolId::Telegram, "telegram:1", request),
            None
        );
    }
}
