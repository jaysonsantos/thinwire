//! Discord official bot/OAuth stub. User-account self-bots are refused.

use super::adapter::{
    AdapterCommand, AdapterError, AdapterStatus, ChatMessage, Conversation, DiscordAuthMode,
    EventTx, ProtocolAdapter, ProtocolCapabilities, ProtocolId, SupportClass, emit_conversation,
    emit_message, emit_status,
};

const CAPABILITIES: ProtocolCapabilities = ProtocolCapabilities {
    id: ProtocolId::Discord,
    support: SupportClass::Constrained,
    short_label: "Constrained · bot/OAuth only",
    detail: "Official bot/OAuth only. No Discord user-account self-bots. Not a reliable personal client.",
    official_api: true,
    allows_user_account_automation: false,
};

const SELF_BOT_REFUSAL: &str = "Discord user-account / self-bot automation is refused. Official bot/OAuth only. License-clean crates do not grant Discord permission to automate a personal account.";

/// Constrained Discord stub. Never starts a user-account client.
#[derive(Debug)]
pub struct DiscordAdapter;

impl DiscordAdapter {
    #[must_use]
    pub const fn capabilities() -> ProtocolCapabilities {
        CAPABILITIES
    }

    /// Explicit refusal used by tests and any future connect UI.
    pub fn connect_user_account() -> Result<(), AdapterError> {
        Err(AdapterError::Refused {
            protocol: ProtocolId::Discord,
            reason: SELF_BOT_REFUSAL,
        })
    }

    fn seed_placeholders(&self, events: &EventTx) {
        emit_status(
            events,
            ProtocolId::Discord,
            AdapterStatus::Stubbed,
            CAPABILITIES.detail,
        );
        emit_conversation(
            events,
            Conversation {
                protocol: ProtocolId::Discord,
                id: "discord:example-guild".into(),
                title: "Example guild #general".into(),
                preview: "Bot/OAuth stub — no user-account session.".into(),
                unread: 1,
            },
        );
        emit_message(
            events,
            ChatMessage {
                protocol: ProtocolId::Discord,
                conversation_id: "discord:example-guild".into(),
                id: "discord:example-guild:1".into(),
                sender: "thinwire".into(),
                body: "Discord is constrained to official bot/OAuth. User-account / self-bot paths are refused.".into(),
                outbound: false,
            },
        );
    }
}

impl ProtocolAdapter for DiscordAdapter {
    fn id(&self) -> ProtocolId {
        ProtocolId::Discord
    }

    fn capabilities(&self) -> ProtocolCapabilities {
        CAPABILITIES
    }

    fn start(&mut self, events: EventTx) {
        tracing::info!("discord adapter start (bot/oauth stub; self-bots refused)");
        self.seed_placeholders(&events);
    }

    fn handle(&mut self, command: AdapterCommand, events: &EventTx) -> Result<(), AdapterError> {
        match command {
            AdapterCommand::ConnectDiscord {
                mode: DiscordAuthMode::UserAccount,
            } => Self::connect_user_account(),
            AdapterCommand::ConnectDiscord {
                mode: DiscordAuthMode::Bot | DiscordAuthMode::OAuth,
            }
            | AdapterCommand::Connect {
                protocol: ProtocolId::Discord,
            } => {
                emit_status(
                    events,
                    ProtocolId::Discord,
                    AdapterStatus::Stubbed,
                    "Discord bot/OAuth stub. No token is stored or requested.",
                );
                Ok(())
            }
            AdapterCommand::Disconnect {
                protocol: ProtocolId::Discord,
            } => {
                emit_status(
                    events,
                    ProtocolId::Discord,
                    AdapterStatus::Stubbed,
                    CAPABILITIES.detail,
                );
                Ok(())
            }
            _ => Err(AdapterError::Unavailable {
                protocol: ProtocolId::Discord,
                reason: "command is not handled by the Discord stub",
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::unbounded_channel;

    #[test]
    fn capabilities_forbid_user_account_automation() {
        let caps = DiscordAdapter::capabilities();
        assert_eq!(caps.support, SupportClass::Constrained);
        assert!(!caps.allows_user_account_automation);
        assert!(caps.short_label.contains("bot/OAuth"));
    }

    #[test]
    fn user_account_self_bot_path_is_refused() {
        let err = DiscordAdapter::connect_user_account().unwrap_err();
        assert!(matches!(
            err,
            AdapterError::Refused {
                protocol: ProtocolId::Discord,
                ..
            }
        ));

        let mut adapter = DiscordAdapter;
        let (tx, mut rx) = unbounded_channel();
        let err = adapter
            .handle(
                AdapterCommand::ConnectDiscord {
                    mode: DiscordAuthMode::UserAccount,
                },
                &tx,
            )
            .unwrap_err();
        assert!(matches!(
            err,
            AdapterError::Refused {
                protocol: ProtocolId::Discord,
                ..
            }
        ));
        assert!(
            rx.try_recv().is_err(),
            "refusal must not emit a connect event"
        );
    }
}
