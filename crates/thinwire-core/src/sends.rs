//! One tracker for the sends and retries of every protocol.
//!
//! A chat has at most one send or retry in flight. Only the adapter's
//! `SendAccepted` / `SendRejected` for the same request id ends it. A status
//! (`Connecting`, `Error`), a command failure, or a notice never does. Only
//! the end of a protocol's session drops its entries.

use std::collections::HashMap;

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

/// Sends and retries in flight, keyed by (protocol, chat).
#[derive(Debug)]
pub(crate) struct SendTracker {
    /// The next request id. Unique for the whole app, never per protocol.
    next: u64,
    open: HashMap<(ProtocolId, String), Pending>,
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
            .map(Pending::request)
    }

    /// Start a send. `None` while this chat already has a send or a retry.
    pub(crate) fn begin_send(
        &mut self,
        protocol: ProtocolId,
        chat: &str,
        body: &str,
    ) -> Option<u64> {
        self.begin(protocol, chat, |request| Pending::Send {
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
    ) -> Option<u64> {
        self.begin(protocol, chat, |request| Pending::Retry {
            request,
            message_id: message_id.to_owned(),
        })
    }

    fn begin(
        &mut self,
        protocol: ProtocolId,
        chat: &str,
        pending: impl FnOnce(u64) -> Pending,
    ) -> Option<u64> {
        let key = (protocol, chat.to_owned());
        if self.open.contains_key(&key) {
            return None;
        }
        let request = self.next;
        self.next += 1;
        self.open.insert(key, pending(request));
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
        if self.open.get(&key).map(Pending::request) != Some(request) {
            return None;
        }
        self.open.remove(&key)
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
            .begin_send(ProtocolId::Slack, "c1", "hi")
            .expect("first");
        assert!(sends.begin_send(ProtocolId::Slack, "c1", "again").is_none());
        assert!(sends.begin_retry(ProtocolId::Slack, "c1", "m1").is_none());
        let other = sends
            .begin_send(ProtocolId::Discord, "c1", "hi")
            .expect("another protocol");
        assert_ne!(first, other, "request ids are unique for the app");
        assert!(sends.in_flight(ProtocolId::Slack, "c1"));
        assert!(!sends.in_flight(ProtocolId::Slack, "c2"));
    }

    #[test]
    fn only_the_matching_request_settles() {
        let mut sends = SendTracker::default();
        let request = sends
            .begin_send(ProtocolId::Telegram, "telegram:1", "hi")
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
            .begin_retry(ProtocolId::WhatsApp, "wa:1", "wa:1:9")
            .expect("retry");
        assert!(
            sends
                .begin_send(ProtocolId::WhatsApp, "wa:1", "new")
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
        sends.begin_send(ProtocolId::Slack, "c1", "a");
        sends.begin_retry(ProtocolId::Slack, "c2", "m");
        sends.begin_send(ProtocolId::Telegram, "telegram:1", "b");
        sends.drop_protocol(ProtocolId::Slack);
        assert!(!sends.in_flight(ProtocolId::Slack, "c1"));
        assert!(!sends.in_flight(ProtocolId::Slack, "c2"));
        assert!(sends.in_flight(ProtocolId::Telegram, "telegram:1"));
    }
}
