//! Test double used to prove workers push events without calling UI APIs.

use super::adapter::{
    AccountState, AdapterCommand, AdapterError, AdapterStatus, ChatMessage, Conversation, Delivery,
    EventTx, ProtocolAdapter, ProtocolCapabilities, ProtocolId, SupportClass, emit_account,
    emit_conversation, emit_message, emit_status,
};

/// Fixed send time for fake messages: 2026-01-02 03:04:05 UTC.
const FAKE_SENT_AT: i64 = 1_767_323_045;

const CAPABILITIES: ProtocolCapabilities = ProtocolCapabilities {
    id: ProtocolId::Telegram,
    support: SupportClass::Supported,
    short_label: "fake adapter",
    detail: "In-process test double. Pushes channel events only.",
    official_api: true,
    allows_user_account_automation: false,
    sends_text: true,
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
        // The shell drops inbox events of an unlinked protocol (ADR 0010):
        // link before the first row, so tests and examples can seed a view.
        emit_account(&events, ProtocolId::Telegram, AccountState::Linked);
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
                        participant: "worker".into(),
                        preview: "Pushed from the worker.".into(),
                        unread: 0,
                        order: 0,
                        last_at: FAKE_SENT_AT,
                        is_group: false,
                        writable: true,
                        placeholder: false,
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
                        delivery: Delivery::Sent,
                        sent_at: FAKE_SENT_AT,
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
                emit_account(events, ProtocolId::Telegram, AccountState::Unlinked);
                Ok(())
            }
            _ => Err(AdapterError::Unavailable {
                protocol: ProtocolId::Telegram,
                reason: "fake adapter does not handle this command",
            }),
        }
    }
}
