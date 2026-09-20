//! Signal unsupported third-party stub (presage / libsignal-style).
//!
//! Experimental. Breakage expected. Not a reliable personal client. No networking.

use super::adapter::{
    AdapterCommand, AdapterError, AdapterStatus, ChatMessage, Conversation, EventTx,
    ProtocolAdapter, ProtocolCapabilities, ProtocolId, SupportClass, emit_conversation,
    emit_message, emit_status,
};

const CAPABILITIES: ProtocolCapabilities = ProtocolCapabilities {
    id: ProtocolId::Signal,
    support: SupportClass::Experimental,
    short_label: "Experimental · breakage expected",
    detail: "No supported third-party client API. presage/libsignal-style path. Breakage expected. Not reliable.",
    official_api: false,
    allows_user_account_automation: false,
};

/// Experimental Signal stub. Does not talk to Signal servers.
#[derive(Debug)]
pub struct SignalAdapter;

impl SignalAdapter {
    #[must_use]
    pub const fn capabilities() -> ProtocolCapabilities {
        CAPABILITIES
    }

    fn seed_placeholders(&self, events: &EventTx) {
        emit_status(
            events,
            ProtocolId::Signal,
            AdapterStatus::Stubbed,
            CAPABILITIES.detail,
        );
        emit_conversation(
            events,
            Conversation {
                protocol: ProtocolId::Signal,
                id: "signal:placeholder".into(),
                title: "Placeholder chat".into(),
                preview: "Unsupported third-party path — not connected.".into(),
                unread: 1,
            },
        );
        emit_message(
            events,
            ChatMessage {
                protocol: ProtocolId::Signal,
                conversation_id: "signal:placeholder".into(),
                id: "signal:placeholder:1".into(),
                sender: "thinwire".into(),
                body: "Signal has no supported third-party client API. Breakage and unsigned clients are expected. This is not a live session.".into(),
                outbound: false,
            },
        );
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
        tracing::info!("signal adapter start (experimental stub)");
        self.seed_placeholders(&events);
    }

    fn handle(&mut self, command: AdapterCommand, events: &EventTx) -> Result<(), AdapterError> {
        match command {
            AdapterCommand::Connect {
                protocol: ProtocolId::Signal,
            }
            | AdapterCommand::Disconnect {
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
            _ => Err(AdapterError::Unavailable {
                protocol: ProtocolId::Signal,
                reason: "command is not handled by the Signal stub",
            }),
        }
    }
}
