//! MIT side of Signal: the stub and the helper adapter.
//!
//! The client that links Presage and libsignal is `thinwire-signal`
//! (AGPL-3.0-only). It runs in the `thinwire-signal-helper` process
//! (ADR 0013, #248). This crate does not depend on it. A `signal-local`
//! build replaces this stub in the host with [`signal_helper_adapter`],
//! which talks to that process over a pipe.

use std::path::PathBuf;
use std::sync::Arc;

use crate::adapter::{
    AdapterCommand, AdapterError, AdapterStatus, EventTx, ProtocolAdapter, ProtocolCapabilities,
    ProtocolId, SupportClass, emit_status, emit_stopped,
};

const CAPABILITY_DETAIL: &str = "Linked secondary device (presage). The signal-local feature is off in this build. No supported third-party API, expect breakage.";

const CAPABILITIES: ProtocolCapabilities = ProtocolCapabilities {
    id: ProtocolId::Signal,
    support: SupportClass::Experimental,
    short_label: "Experimental · expect breakage",
    detail: CAPABILITY_DETAIL,
    official_api: false,
    allows_user_account_automation: false,
    sends_text: true,
    // The Signal adapter answers LoadOlderMessages (ADR 0010 rule 10).
    pages_history: true,
};

/// File name of the Signal helper program, without `.exe`.
pub const SIGNAL_HELPER_PROGRAM: &str = "thinwire-signal-helper";

/// The phone removed this linked device. The account row and the open chat
/// show this line. Sending stays off. Local history stays.
pub const SIGNAL_DEVICE_REMOVED: &str = "Signal removed this device. Link again";

/// The account row shows this after one unauthorized receive open, while the
/// worker waits and opens the stream once more. It is not removal. The row
/// goes back to its normal line if that open succeeds or fails another way.
pub const SIGNAL_LINK_CHECK: &str = "Checking Signal link…";

/// A previous Signal number after you link a different number.
/// The old account row and its open chat show this line. Sending stays off.
/// The history stays on this computer. Signal has no supported third-party
/// API. Expect breakage.
pub const SIGNAL_PREVIOUS_ACCOUNT: &str =
    "You linked a different number. This history stays. Sending is off.";

/// The helper is about to publish `retained/` again.
/// The core keeps the rows that are already on screen.
/// A slot whose store this publish could not read stays.
/// A slot this publish does not send is dropped when the publish settles.
/// This line is not a status of the live account.
pub const SIGNAL_RETAINED_RESET: &str = "Signal is loading previous history.";

/// The helper finished the retained publish for this link.
/// The core drops a selected slot that was not published and whose store
/// this publish could read. A store that could not be read keeps its slot.
/// This line is not a status of the live account.
pub const SIGNAL_RETAINED_SETTLED: &str = "Signal finished loading previous history.";

/// This publish could not read one previous-account store.
/// The files stay on disk. The core keeps that slot.
/// The row stays in the list. The row is disabled.
/// The row has no Send control and no composer.
/// The row shows this sentence. The sentence has no slot count.
/// The wire detail is this sentence, one space, and the slot count.
/// The core does not show the slot count. The helper log keeps the count.
/// The slot is not an account id.
/// This line is not a status of the live account.
pub const SIGNAL_RETAINED_UNREADABLE: &str = "This history could not be read. It was not deleted.";

/// Status detail for one previous-account store this publish could not read.
#[must_use]
pub fn signal_retained_unreadable_detail(slot: u32) -> String {
    format!("{SIGNAL_RETAINED_UNREADABLE} {slot}")
}

/// The slot in a [`signal_retained_unreadable_detail`] line.
/// `None` when `detail` is a different status. The slot is a count.
#[must_use]
pub fn signal_retained_unreadable_slot(detail: &str) -> Option<u32> {
    let rest = detail
        .strip_prefix(SIGNAL_RETAINED_UNREADABLE)?
        .strip_prefix(' ')?;
    if rest.is_empty() || rest.starts_with('+') || (rest.len() > 1 && rest.starts_with('0')) {
        return None;
    }
    if !rest.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    rest.parse().ok().filter(|slot: &u32| *slot > 0)
}

/// Chat id prefix for a previous Signal account in `retained/`.
/// The form is `retained:<slot>:<chat id>`. The slot is a count, not an
/// account id. The helper writes the count when it moves a store aside.
/// A later link keeps that count.
const SIGNAL_RETAINED_MARK: &str = "retained:";

/// Chat id for one chat of a previous Signal account.
/// `slot` starts at 1. `chat_id` is the live chat id.
#[must_use]
pub fn signal_retained_chat_id(slot: u32, chat_id: &str) -> String {
    format!("{SIGNAL_RETAINED_MARK}{slot}:{chat_id}")
}

/// The chat id inside a retained id, with the slot prefix removed.
/// `None` for a live chat. The result is not an account id.
#[must_use]
pub fn signal_retained_chat(conversation_id: &str) -> Option<&str> {
    signal_retained_slot(conversation_id)?;
    let rest = conversation_id.strip_prefix(SIGNAL_RETAINED_MARK)?;
    rest.split_once(':').map(|(_, chat)| chat)
}

/// The previous-account slot of `conversation_id`, when it has one.
/// `None` for a live chat. The slot is not an account id.
#[must_use]
pub fn signal_retained_slot(conversation_id: &str) -> Option<u32> {
    let rest = conversation_id.strip_prefix(SIGNAL_RETAINED_MARK)?;
    let (slot, chat) = rest.split_once(':')?;
    if chat.is_empty() || slot.is_empty() || slot.starts_with('+') {
        return None;
    }
    if slot.len() > 1 && slot.starts_with('0') {
        return None;
    }
    slot.parse().ok().filter(|slot| *slot > 0)
}

/// The capabilities of the client in the helper. The account row shows them
/// also while no helper runs.
const HELPER_CAPABILITIES: ProtocolCapabilities = ProtocolCapabilities {
    id: ProtocolId::Signal,
    support: SupportClass::Experimental,
    short_label: "Experimental · expect breakage",
    detail: "Linked secondary device (presage) in the thinwire-signal-helper process. Experimental. No supported third-party API, expect breakage.",
    official_api: false,
    allows_user_account_automation: false,
    sends_text: true,
    pages_history: true,
};

/// The Signal adapter of a `signal-local` build: it runs the AGPL client in
/// the `thinwire-signal-helper` process (ADR 0013).
///
/// `configured` is the helper path from the settings, used when no helper is
/// next to the app binary. Signal links with a QR only, so no phone number
/// goes to this helper.
#[must_use]
pub fn signal_helper_adapter(configured: Option<PathBuf>) -> crate::helper::HelperAdapter {
    crate::helper::HelperAdapter::new(
        crate::helper::HelperSpec {
            protocol: ProtocolId::Signal,
            capabilities: HELPER_CAPABILITIES,
        },
        Arc::new(crate::helper::ProcessLauncher::new(
            SIGNAL_HELPER_PROGRAM,
            configured,
        )),
        None,
    )
}

const FEATURE_OFF: &str = "signal-local is off in this build. No Signal session is started.";

const NOTICE_REQUIRED: &str =
    "Signal linking is refused until the full-screen Signal notice is accepted";

/// Feature-off Signal adapter. It never opens a session.
pub struct SignalAdapter {
    notice_accepted: bool,
}

impl SignalAdapter {
    #[must_use]
    pub fn new() -> Self {
        Self {
            notice_accepted: false,
        }
    }

    #[must_use]
    pub const fn capabilities() -> ProtocolCapabilities {
        CAPABILITIES
    }

    fn acknowledge(&mut self, events: &EventTx) -> Result<(), AdapterError> {
        self.notice_accepted = true;
        emit_status(
            events,
            ProtocolId::Signal,
            AdapterStatus::Stubbed,
            "Signal notice accepted. No session has started.",
        );
        Ok(())
    }

    fn begin_link(&mut self, events: &EventTx) -> Result<(), AdapterError> {
        if !self.notice_accepted {
            return Err(AdapterError::Refused {
                protocol: ProtocolId::Signal,
                reason: NOTICE_REQUIRED,
            });
        }
        let _ = events;
        Err(AdapterError::Unavailable {
            protocol: ProtocolId::Signal,
            reason: FEATURE_OFF,
        })
    }

    fn cancel_link(&mut self, events: &EventTx) -> Result<(), AdapterError> {
        self.notice_accepted = false;
        emit_status(
            events,
            ProtocolId::Signal,
            AdapterStatus::Stubbed,
            "Signal linking cancelled. No session is running.",
        );
        Ok(())
    }
}

impl Default for SignalAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl ProtocolAdapter for SignalAdapter {
    fn id(&self) -> ProtocolId {
        ProtocolId::Signal
    }

    fn capabilities(&self) -> ProtocolCapabilities {
        CAPABILITIES
    }

    fn start(&mut self, events: EventTx) {
        tracing::info!("signal adapter start");
        emit_status(
            &events,
            ProtocolId::Signal,
            AdapterStatus::Stubbed,
            CAPABILITIES.detail,
        );
    }

    fn shutdown(&mut self, events: &EventTx) {
        self.notice_accepted = false;
        emit_stopped(events, ProtocolId::Signal);
    }

    fn handle(&mut self, command: AdapterCommand, events: &EventTx) -> Result<(), AdapterError> {
        match command {
            AdapterCommand::Connect {
                protocol: ProtocolId::Signal,
            } => {
                emit_status(
                    events,
                    ProtocolId::Signal,
                    AdapterStatus::Stubbed,
                    CAPABILITIES.detail,
                );
                Ok(())
            }
            AdapterCommand::Disconnect {
                protocol: ProtocolId::Signal,
            }
            | AdapterCommand::SignalCancelLink => self.cancel_link(events),
            AdapterCommand::SignalAcknowledgeNotice => self.acknowledge(events),
            AdapterCommand::SignalBeginLink { .. } => self.begin_link(events),
            _ => Err(AdapterError::Unavailable {
                protocol: ProtocolId::Signal,
                reason: FEATURE_OFF,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_retained_chat_id_keeps_the_slot_and_the_chat() {
        let group = "signal:group:abcd";
        let id = signal_retained_chat_id(2, group);
        assert_eq!(signal_retained_slot(&id), Some(2));
        assert_eq!(signal_retained_chat(&id), Some(group));
        assert!(id.ends_with(group));
        assert_eq!(signal_retained_slot("ada"), None);
        assert_eq!(signal_retained_chat("ada"), None);
        assert_eq!(signal_retained_slot("retained:0:ada"), None);
        assert_eq!(signal_retained_slot("retained:01:ada"), None);
        assert!(
            !SIGNAL_PREVIOUS_ACCOUNT
                .to_ascii_lowercase()
                .contains("reliable"),
            "the row must not call Signal reliable"
        );
        assert!(SIGNAL_PREVIOUS_ACCOUNT.contains("Sending is off."));
        assert!(
            !SIGNAL_PREVIOUS_ACCOUNT
                .to_ascii_lowercase()
                .contains("link again"),
            "the previous-account line does not ask the user to link"
        );
    }

    #[test]
    fn a_unreadable_retained_slot_is_a_count() {
        assert_eq!(
            signal_retained_unreadable_slot(&signal_retained_unreadable_detail(4)),
            Some(4)
        );
        assert_eq!(
            SIGNAL_RETAINED_UNREADABLE,
            "This history could not be read. It was not deleted."
        );
        assert!(
            !SIGNAL_RETAINED_UNREADABLE
                .chars()
                .any(|ch| ch.is_ascii_digit()),
            "the user-facing text has no slot count"
        );
        assert_eq!(
            signal_retained_unreadable_slot(SIGNAL_RETAINED_UNREADABLE),
            None
        );
        assert_eq!(
            signal_retained_unreadable_slot(&format!("{SIGNAL_RETAINED_UNREADABLE} 0")),
            None
        );
        assert_eq!(
            signal_retained_unreadable_slot(&format!("{SIGNAL_RETAINED_UNREADABLE} 01")),
            None
        );
        assert!(
            !SIGNAL_RETAINED_UNREADABLE
                .to_ascii_lowercase()
                .contains("reliable"),
            "the unreadable line must not call Signal reliable"
        );
    }
}
