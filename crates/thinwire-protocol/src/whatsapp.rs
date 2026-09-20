//! WhatsApp unofficial linked-device stub (ZapFast / whatsapp-rust inspired).
//!
//! Experimental. ToS / ban risk. No networking.

use super::adapter::{
    AdapterCommand, AdapterError, AdapterStatus, ChatMessage, Conversation, EventTx,
    ProtocolAdapter, ProtocolCapabilities, ProtocolId, SupportClass, emit_conversation,
    emit_message, emit_status,
};

const CAPABILITIES: ProtocolCapabilities = ProtocolCapabilities {
    id: ProtocolId::WhatsApp,
    support: SupportClass::Experimental,
    short_label: "Experimental · ban risk",
    detail: "Unofficial Web / linked-device style (ZapFast / whatsapp-rust inspired). Ban risk.",
    official_api: false,
    allows_user_account_automation: false,
};

/// Experimental WhatsApp stub. Does not open a linked-device session.
#[derive(Debug)]
pub struct WhatsAppAdapter;

impl WhatsAppAdapter {
    #[must_use]
    pub const fn capabilities() -> ProtocolCapabilities {
        CAPABILITIES
    }

    fn seed_placeholders(&self, events: &EventTx) {
        emit_status(
            events,
            ProtocolId::WhatsApp,
            AdapterStatus::Stubbed,
            CAPABILITIES.detail,
        );
        emit_conversation(
            events,
            Conversation {
                protocol: ProtocolId::WhatsApp,
                id: "whatsapp:placeholder".into(),
                title: "Placeholder chat".into(),
                participant: "Placeholder contact".into(),
                preview: "Experimental unofficial path — not connected.".into(),
                unread: 1,
            },
        );
        emit_message(
            events,
            ChatMessage {
                protocol: ProtocolId::WhatsApp,
                conversation_id: "whatsapp:placeholder".into(),
                id: "whatsapp:placeholder:1".into(),
                sender: "thinwire".into(),
                body: "WhatsApp is experimental. Unofficial linked-device code can get a personal account banned. This is not a live session.".into(),
                outbound: false,
            },
        );
    }
}

impl ProtocolAdapter for WhatsAppAdapter {
    fn id(&self) -> ProtocolId {
        ProtocolId::WhatsApp
    }

    fn capabilities(&self) -> ProtocolCapabilities {
        CAPABILITIES
    }

    fn start(&mut self, events: EventTx) {
        tracing::info!("whatsapp adapter start (experimental stub)");
        self.seed_placeholders(&events);
    }

    fn handle(&mut self, command: AdapterCommand, events: &EventTx) -> Result<(), AdapterError> {
        match command {
            AdapterCommand::Connect {
                protocol: ProtocolId::WhatsApp,
            }
            | AdapterCommand::Disconnect {
                protocol: ProtocolId::WhatsApp,
            } => {
                emit_status(
                    events,
                    ProtocolId::WhatsApp,
                    AdapterStatus::Stubbed,
                    CAPABILITIES.detail,
                );
                Ok(())
            }
            _ => Err(AdapterError::Unavailable {
                protocol: ProtocolId::WhatsApp,
                reason: "command is not handled by the WhatsApp stub",
            }),
        }
    }
}
