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

/// Shown in the account list while old Signal history is on disk and its
/// rows are not on screen. The rows come back after the next link.
///
/// Signal does not show an account phone number, and no phone number
/// crosses the helper pipe, so this line names no digits.
pub const SIGNAL_HISTORY_KEPT: &str =
    "Old Signal history for the previous number is kept. It shows again after you link.";

/// The helper is about to publish `retained/` again.
/// The core drops the previous retained rows first, then the new rows arrive.
/// This line is not a status of the live account.
pub const SIGNAL_RETAINED_RESET: &str = "Signal is loading previous history.";

/// The helper finished the retained publish for this link.
/// The core drops a selected slot that was not published.
/// This line is not a status of the live account.
pub const SIGNAL_RETAINED_SETTLED: &str = "Signal finished loading previous history.";

/// No previous account remains under `retained/`.
/// The core clears the held line. This line is not a status of the live account.
pub const SIGNAL_HISTORY_NOT_KEPT: &str = "No old Signal history is waiting.";

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
            !SIGNAL_HISTORY_KEPT
                .to_ascii_lowercase()
                .contains("reliable"),
            "the held line must not call Signal reliable"
        );
        assert!(SIGNAL_HISTORY_KEPT.contains("It shows again after you link."));
        assert!(
            !SIGNAL_HISTORY_KEPT.chars().any(|ch| ch.is_ascii_digit()),
            "the held line has no phone number"
        );
    }
}
