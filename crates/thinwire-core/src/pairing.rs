//! The pairing that runs for each helper account (WhatsApp, Signal).
//!
//! Each start of a pairing gets a new id from the shell. A QR code or a pair
//! code carries the id of its pairing, and only the payload of the pairing
//! that runs now shows. Each account has its own entry: the end of a
//! WhatsApp pairing does not end a Signal link, and the reverse.

use std::collections::HashMap;

use thinwire_protocol::ProtocolId;

/// The ids of the pairings that run now.
#[derive(Debug)]
pub(crate) struct Pairings {
    /// The id of the next pairing. It starts at 1: 0 means "no pairing".
    next: u64,
    running: HashMap<ProtocolId, u64>,
}

impl Default for Pairings {
    fn default() -> Self {
        Self {
            next: 1,
            running: HashMap::new(),
        }
    }
}

impl Pairings {
    /// Start a pairing of `account`. The answer is its id. An earlier
    /// pairing of that account ends: its later payloads do not show.
    pub(crate) fn begin(&mut self, account: ProtocolId) -> u64 {
        let generation = self.next;
        self.next += 1;
        self.running.insert(account, generation);
        generation
    }

    /// No pairing of `account` runs any more.
    pub(crate) fn end(&mut self, account: ProtocolId) {
        self.running.remove(&account);
    }

    /// The id of the pairing of `account` that runs now. `0` when none runs.
    pub(crate) fn running(&self, account: ProtocolId) -> u64 {
        self.running.get(&account).copied().unwrap_or(0)
    }

    /// A pairing payload with the id `generation` belongs to the pairing of
    /// `account` that runs now.
    pub(crate) fn is_current(&self, account: ProtocolId, generation: u64) -> bool {
        let current = self.running.get(&account) == Some(&generation);
        if !current {
            tracing::warn!("pairing payload dropped: it is from an older pairing");
        }
        current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_payload_of_the_pairing_that_runs_now_is_current() {
        let mut pairings = Pairings::default();
        assert_eq!(pairings.running(ProtocolId::WhatsApp), 0);
        assert!(!pairings.is_current(ProtocolId::WhatsApp, 0), "none runs");
        let first = pairings.begin(ProtocolId::WhatsApp);
        assert_eq!(first, 1);
        assert!(pairings.is_current(ProtocolId::WhatsApp, first));
        let second = pairings.begin(ProtocolId::WhatsApp);
        assert_ne!(first, second);
        assert!(!pairings.is_current(ProtocolId::WhatsApp, first));
        assert!(pairings.is_current(ProtocolId::WhatsApp, second));
        assert_eq!(pairings.running(ProtocolId::WhatsApp), second);
    }

    #[test]
    fn the_end_of_one_pairing_does_not_end_the_pairing_of_another_account() {
        let mut pairings = Pairings::default();
        let whatsapp = pairings.begin(ProtocolId::WhatsApp);
        let signal = pairings.begin(ProtocolId::Signal);
        assert_ne!(whatsapp, signal, "an id is never used two times");
        assert!(!pairings.is_current(ProtocolId::Signal, whatsapp));
        pairings.end(ProtocolId::WhatsApp);
        assert!(!pairings.is_current(ProtocolId::WhatsApp, whatsapp));
        assert!(pairings.is_current(ProtocolId::Signal, signal));
        // A new pairing after an end gets a new id.
        assert!(pairings.begin(ProtocolId::WhatsApp) > signal);
    }
}
