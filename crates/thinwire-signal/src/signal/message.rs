// SPDX-License-Identifier: AGPL-3.0-only
#![cfg_attr(not(feature = "signal-local"), allow(dead_code))]
//! Which Signal envelopes become a visible chat line.

/// Text the inbox can show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VisibleText<'a> {
    pub(crate) body: &'a str,
    pub(crate) outbound: bool,
}

/// The two envelopes that carry a chat body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Incoming<'a> {
    /// A message sent to this device.
    Data(Option<&'a str>),
    /// A transcript of a message this account sent from another device.
    SentSync(Option<&'a str>),
    Other,
}

#[must_use]
pub(crate) fn visible_text(incoming: Incoming<'_>) -> Option<VisibleText<'_>> {
    match incoming {
        Incoming::Data(Some(body)) => Some(VisibleText {
            body,
            outbound: false,
        }),
        Incoming::SentSync(Some(body)) => Some(VisibleText {
            body,
            outbound: true,
        }),
        Incoming::Data(None) | Incoming::SentSync(None) | Incoming::Other => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sent_sync_is_an_outgoing_message() {
        let shown = visible_text(Incoming::SentSync(Some("from the phone"))).expect("text");
        assert!(shown.outbound);
        assert_eq!(shown.body, "from the phone");
        assert!(visible_text(Incoming::SentSync(None)).is_none());
    }

    #[test]
    fn a_data_message_stays_incoming() {
        let shown = visible_text(Incoming::Data(Some("hello"))).expect("text");
        assert!(!shown.outbound);
        assert!(visible_text(Incoming::Other).is_none());
    }
}
