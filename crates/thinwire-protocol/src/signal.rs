//! MIT stub for Signal.
//!
//! The client that links Presage and libsignal is `thinwire-signal`
//! (AGPL-3.0-only). This crate does not depend on it. A `signal-local` build
//! replaces this stub in the host.

use crate::adapter::{
    AdapterCommand, AdapterError, AdapterStatus, EventTx, ProtocolAdapter, ProtocolCapabilities,
    ProtocolId, SupportClass, emit_status, emit_stopped,
};

const CAPABILITY_DETAIL: &str = "Local-only secondary device (presage). The signal-local feature is off in this build. Not in release builds.";

const CAPABILITIES: ProtocolCapabilities = ProtocolCapabilities {
    id: ProtocolId::Signal,
    support: SupportClass::Experimental,
    short_label: "Local only · AGPL · not in releases",
    detail: CAPABILITY_DETAIL,
    official_api: false,
    allows_user_account_automation: false,
    sends_text: true,
};

const FEATURE_OFF: &str = "signal-local is off in this build. No Signal session is started.";

const NOTICE_REQUIRED: &str =
    "Signal linking is refused until the full-screen local-build notice is accepted";

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
            "Signal local-build notice accepted. No session has started.",
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
