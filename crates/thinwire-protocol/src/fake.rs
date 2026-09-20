//! Test double used to prove workers push events without calling UI APIs.

use super::adapter::{
    AdapterCommand, AdapterError, AdapterStatus, ChatMessage, Conversation, EventTx,
    ProtocolAdapter, ProtocolCapabilities, ProtocolId, SupportClass, emit_conversation,
    emit_message, emit_status,
};

const CAPABILITIES: ProtocolCapabilities = ProtocolCapabilities {
    id: ProtocolId::Telegram,
    support: SupportClass::Supported,
    short_label: "fake adapter",
    detail: "In-process test double. Pushes channel events only.",
    official_api: true,
    allows_user_account_automation: false,
};

/// Worker-side fake. Deliberately has no egui/eframe imports.
#[derive(Debug, Default)]
pub struct FakeAdapter {
    started: bool,
}

impl FakeAdapter {
    #[must_use]
    pub const fn new() -> Self {
        Self { started: false }
    }

    #[must_use]
    pub const fn started(&self) -> bool {
        self.started
    }
}

impl ProtocolAdapter for FakeAdapter {
    fn id(&self) -> ProtocolId {
        ProtocolId::Telegram
    }

    fn capabilities(&self) -> ProtocolCapabilities {
        CAPABILITIES
    }

    fn start(&mut self, events: EventTx) {
        self.started = true;
        emit_status(
            &events,
            ProtocolId::Telegram,
            AdapterStatus::Ready,
            "fake adapter ready",
        );
    }

    fn handle(&mut self, command: AdapterCommand, events: &EventTx) -> Result<(), AdapterError> {
        match command {
            AdapterCommand::Connect {
                protocol: ProtocolId::Telegram,
            } => {
                emit_conversation(
                    events,
                    Conversation {
                        protocol: ProtocolId::Telegram,
                        id: "fake:chat".into(),
                        title: "Fake chat".into(),
                        preview: "Pushed from the worker.".into(),
                        unread: 0,
                    },
                );
                emit_message(
                    events,
                    ChatMessage {
                        protocol: ProtocolId::Telegram,
                        conversation_id: "fake:chat".into(),
                        id: "fake:chat:1".into(),
                        sender: "worker".into(),
                        body: "event from tokio worker".into(),
                        outbound: false,
                    },
                );
                Ok(())
            }
            AdapterCommand::Disconnect {
                protocol: ProtocolId::Telegram,
            } => {
                emit_status(
                    events,
                    ProtocolId::Telegram,
                    AdapterStatus::Stubbed,
                    "fake adapter disconnected",
                );
                Ok(())
            }
            _ => Err(AdapterError::Unavailable {
                protocol: ProtocolId::Telegram,
                reason: "fake adapter does not handle this command",
            }),
        }
    }
}
