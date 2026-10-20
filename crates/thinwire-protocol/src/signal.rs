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
