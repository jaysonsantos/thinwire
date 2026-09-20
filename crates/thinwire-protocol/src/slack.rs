//! Slack official OAuth / workspace-app stub. Not a personal desktop clone.

use super::adapter::{
    AdapterCommand, AdapterError, AdapterStatus, ChatMessage, Conversation, EventTx,
    ProtocolAdapter, ProtocolCapabilities, ProtocolId, SupportClass, emit_conversation,
    emit_message, emit_status,
};

const CAPABILITIES: ProtocolCapabilities = ProtocolCapabilities {
    id: ProtocolId::Slack,
    support: SupportClass::Supported,
    short_label: "Supported · OAuth-only",
    detail: "Official Slack OAuth / workspace app. Supported goal. Not a personal desktop clone. No token in the repo.",
    official_api: true,
    allows_user_account_automation: false,
};

/// Official Slack path. OAuth login is out of scope for this revision.
#[derive(Debug)]
pub struct SlackAdapter;

impl SlackAdapter {
    #[must_use]
    pub const fn capabilities() -> ProtocolCapabilities {
        CAPABILITIES
    }

    fn seed_placeholders(&self, events: &EventTx) {
        emit_status(
            events,
            ProtocolId::Slack,
            AdapterStatus::Stubbed,
            CAPABILITIES.detail,
        );
        emit_conversation(
            events,
            Conversation {
                protocol: ProtocolId::Slack,
                id: "slack:example-channel".into(),
                title: "#general".into(),
                participant: "workspace".into(),
                preview: "OAuth workspace stub — not connected.".into(),
                unread: 1,
            },
        );
        emit_message(
            events,
            ChatMessage {
                protocol: ProtocolId::Slack,
                conversation_id: "slack:example-channel".into(),
                id: "slack:example-channel:1".into(),
                sender: "thinwire".into(),
                body: "Slack is a supported OAuth / workspace-app goal. This pane is placeholder data; no workspace token is stored.".into(),
                outbound: false,
            },
        );
    }
}

impl ProtocolAdapter for SlackAdapter {
    fn id(&self) -> ProtocolId {
        ProtocolId::Slack
    }

    fn capabilities(&self) -> ProtocolCapabilities {
        CAPABILITIES
    }

    fn start(&mut self, events: EventTx) {
        tracing::info!("slack adapter start (oauth stub)");
        self.seed_placeholders(&events);
    }

    fn handle(&mut self, command: AdapterCommand, events: &EventTx) -> Result<(), AdapterError> {
        match command {
            AdapterCommand::Connect {
                protocol: ProtocolId::Slack,
            }
            | AdapterCommand::Disconnect {
                protocol: ProtocolId::Slack,
            } => {
                emit_status(
                    events,
                    ProtocolId::Slack,
                    AdapterStatus::Stubbed,
                    CAPABILITIES.detail,
                );
                Ok(())
            }
            _ => Err(AdapterError::Unavailable {
                protocol: ProtocolId::Slack,
                reason: "command is not handled by the Slack stub",
            }),
        }
    }
}
