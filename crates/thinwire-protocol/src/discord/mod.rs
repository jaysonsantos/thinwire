//! Discord bot/OAuth inbox spike. User-account self-bots are refused.
//!
//! Feature `discord-bot` compiles the twilight bot HTTP client and a guild
//! inbox placeholder. Default builds stay "not ready" and do not emit that
//! placeholder. The HTTP client is constructed on the adapter worker only.
//! `Client::new` does not send a request or open a gateway.

mod install;
mod token;

use std::fmt;
use std::sync::Arc;

use super::adapter::{
    AdapterCommand, AdapterError, AdapterStatus, DiscordAuthMode, EventTx, ProtocolAdapter,
    ProtocolCapabilities, ProtocolId, SupportClass, emit_status,
};
#[cfg(feature = "discord-bot")]
use super::adapter::{ChatMessage, Conversation, emit_conversation, emit_message};
use token::{DiscordTokenKind, authorization_token, classify_discord_token};

pub use install::DiscordOAuthInstall;
pub use token::{
    DISCORD_SECRET_BOT_TOKEN, DISCORD_SECRET_SERVICE, DiscordSecretVault, MemoryDiscordVault,
};

#[cfg(not(feature = "discord-bot"))]
const CAPABILITY_DETAIL: &str = "Bot/OAuth inbox only. Not ready in this build. Enable feature discord-bot. No user-account self-bots or personal DMs.";

#[cfg(feature = "discord-bot")]
const CAPABILITY_DETAIL: &str = "Bot/OAuth guild inbox placeholder (twilight). Gateway is not started. No user-account self-bots or personal DMs.";

const NOT_READY_DETAIL: &str = "Discord bot inbox is not ready in this build. Enable feature discord-bot. No user-account client is compiled.";

const SELF_BOT_REFUSAL: &str = "Discord user-account / self-bot automation is refused. Bot/OAuth only. License-clean crates do not grant Discord permission to automate a personal account.";

const USER_TOKEN_REFUSAL: &str = "Discord user-account tokens are refused. The keychain slot accepts a bot token or an OAuth bearer token.";

const CAPABILITIES: ProtocolCapabilities = ProtocolCapabilities {
    id: ProtocolId::Discord,
    support: SupportClass::Constrained,
    short_label: "Constrained · bot/OAuth inbox only",
    detail: CAPABILITY_DETAIL,
    official_api: true,
    allows_user_account_automation: false,
};

#[cfg(feature = "discord-bot")]
const PLACEHOLDER_CONVERSATION: &str = "discord:guild-inbox:general";

#[cfg(feature = "discord-bot")]
struct BotHttp {
    client: twilight_http::Client,
}

#[cfg(feature = "discord-bot")]
impl fmt::Debug for BotHttp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let _ = &self.client;
        f.write_str("BotHttp { token: <redacted>, gateway: not started }")
    }
}

#[cfg(feature = "discord-bot")]
enum TokenGate {
    Missing,
    Accepted(DiscordTokenKind),
}

/// Constrained Discord adapter. Never starts a user-account client.
pub struct DiscordAdapter {
    secrets: Arc<dyn DiscordSecretVault>,
    #[cfg(feature = "discord-bot")]
    http: Option<BotHttp>,
}

impl DiscordAdapter {
    #[must_use]
    pub fn new(secrets: Arc<dyn DiscordSecretVault>) -> Self {
        Self {
            secrets,
            #[cfg(feature = "discord-bot")]
            http: None,
        }
    }

    #[must_use]
    pub fn memory() -> Self {
        Self::new(Arc::new(MemoryDiscordVault::new()))
    }

    #[must_use]
    pub const fn capabilities() -> ProtocolCapabilities {
        CAPABILITIES
    }

    /// True when feature `discord-bot` compiled the twilight bot inbox.
    pub const fn bot_inbox_compiled() -> bool {
        cfg!(feature = "discord-bot")
    }

    /// Explicit refusal used by tests and any future connect UI.
    pub fn connect_user_account() -> Result<(), AdapterError> {
        Err(AdapterError::Refused {
            protocol: ProtocolId::Discord,
            reason: SELF_BOT_REFUSAL,
        })
    }

    fn prepared_token(&self) -> Result<Option<(DiscordTokenKind, String)>, AdapterError> {
        let Some(raw) = self.secrets.bot_token() else {
            return Ok(None);
        };
        let kind = classify_discord_token(&raw).map_err(|()| AdapterError::Refused {
            protocol: ProtocolId::Discord,
            reason: USER_TOKEN_REFUSAL,
        })?;
        let token = authorization_token(&raw).map_err(|()| AdapterError::Refused {
            protocol: ProtocolId::Discord,
            reason: USER_TOKEN_REFUSAL,
        })?;
        Ok(Some((kind, token)))
    }

    fn connect_bot_inbox(&mut self, events: &EventTx) -> Result<(), AdapterError> {
        let prepared = self.prepared_token()?;
        self.finish_connect(events, prepared)
    }

    #[cfg(not(feature = "discord-bot"))]
    fn finish_connect(
        &mut self,
        events: &EventTx,
        prepared: Option<(DiscordTokenKind, String)>,
    ) -> Result<(), AdapterError> {
        drop(prepared);
        self.clear_http();
        emit_status(
            events,
            ProtocolId::Discord,
            AdapterStatus::Stubbed,
            NOT_READY_DETAIL,
        );
        Ok(())
    }

    #[cfg(feature = "discord-bot")]
    fn finish_connect(
        &mut self,
        events: &EventTx,
        prepared: Option<(DiscordTokenKind, String)>,
    ) -> Result<(), AdapterError> {
        let gate = match &prepared {
            None => TokenGate::Missing,
            Some((kind, _)) => TokenGate::Accepted(*kind),
        };
        self.arm_http(prepared);
        self.emit_placeholder(events, &gate);
        Ok(())
    }

    fn clear_http(&mut self) {
        #[cfg(feature = "discord-bot")]
        {
            self.http = None;
        }
    }

    #[cfg(feature = "discord-bot")]
    fn arm_http(&mut self, prepared: Option<(DiscordTokenKind, String)>) {
        let _ = install::inbox_intents();
        let _ = install::inbox_permissions();
        let Some((_kind, token)) = prepared else {
            self.http = None;
            return;
        };
        // Stores the token on the tokio worker. Does not send HTTP or open a gateway.
        // The ratelimiter inside `Client::new` requires that worker's runtime.
        let _ = rustls::crypto::ring::default_provider().install_default();
        self.http = Some(BotHttp {
            client: twilight_http::Client::new(token),
        });
    }

    #[cfg(feature = "discord-bot")]
    fn emit_placeholder(&self, events: &EventTx, gate: &TokenGate) {
        let token_state = match gate {
            TokenGate::Missing => "bot token is not in the OS keychain",
            TokenGate::Accepted(DiscordTokenKind::Bot) => "bot token is in the OS keychain",
            TokenGate::Accepted(DiscordTokenKind::OAuthBearer) => {
                "OAuth bearer token is in the OS keychain"
            }
        };
        let detail = format!(
            "Discord bot inbox placeholder. {token_state}. Gateway is not started. Not a personal Discord client."
        );
        emit_status(events, ProtocolId::Discord, AdapterStatus::Stubbed, detail);
        emit_conversation(
            events,
            Conversation {
                protocol: ProtocolId::Discord,
                id: PLACEHOLDER_CONVERSATION.into(),
                title: "Bot inbox #general".into(),
                participant: "guild channel".into(),
                preview: "Bot/OAuth guild inbox placeholder. Not a personal Discord client.".into(),
                unread: 1,
                order: 0,
            },
        );
        emit_message(
            events,
            ChatMessage {
                protocol: ProtocolId::Discord,
                conversation_id: PLACEHOLDER_CONVERSATION.into(),
                id: "discord:guild-inbox:general:1".into(),
                sender: "thinwire".into(),
                body: "Guild bot inbox placeholder. The gateway is not started. User-account and self-bot paths are refused.".into(),
                outbound: false,
            },
        );
    }
}

impl fmt::Debug for DiscordAdapter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DiscordAdapter")
            .field("bot_inbox", &Self::bot_inbox_compiled())
            .field("token", &"<redacted>")
            .finish()
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
        tracing::info!(
            bot_inbox = Self::bot_inbox_compiled(),
            "discord adapter start (bot/oauth inbox; self-bots refused)"
        );
        if let Err(error) = self.connect_bot_inbox(&events) {
            emit_status(
                &events,
                ProtocolId::Discord,
                AdapterStatus::Refused,
                error.to_string(),
            );
        }
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
            } => self.connect_bot_inbox(events),
            AdapterCommand::Disconnect {
                protocol: ProtocolId::Discord,
            } => {
                self.clear_http();
                let detail = if Self::bot_inbox_compiled() {
                    "Discord bot inbox disconnected. Gateway was not started."
                } else {
                    NOT_READY_DETAIL
                };
                emit_status(events, ProtocolId::Discord, AdapterStatus::Stubbed, detail);
                Ok(())
            }
            _ => Err(AdapterError::Unavailable {
                protocol: ProtocolId::Discord,
                reason: "command is not handled by the Discord adapter",
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::unbounded_channel;

    fn drain(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::AdapterEvent>,
    ) -> Vec<crate::AdapterEvent> {
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    #[test]
    fn capabilities_forbid_user_account_automation() {
        let caps = DiscordAdapter::capabilities();
        assert_eq!(caps.support, SupportClass::Constrained);
        assert!(!caps.allows_user_account_automation);
        assert!(caps.short_label.contains("bot/OAuth"));
        assert!(!caps.detail.to_ascii_lowercase().contains("reliable"));
        assert!(!caps.detail.contains("personal Discord client is"));
        if DiscordAdapter::bot_inbox_compiled() {
            assert!(caps.detail.contains("placeholder"));
            assert!(!caps.detail.to_ascii_lowercase().contains("not ready"));
        } else {
            assert!(caps.detail.to_ascii_lowercase().contains("not ready"));
        }
    }

    #[test]
    fn feature_off_connect_is_not_ready_without_a_placeholder() {
        let mut adapter = DiscordAdapter::memory();
        let (tx, mut rx) = unbounded_channel();
        adapter.start(tx.clone());
        if DiscordAdapter::bot_inbox_compiled() {
            return;
        }
        let events = drain(&mut rx);
        assert!(events.iter().any(|event| match event {
            crate::AdapterEvent::Status { detail, status, .. } => {
                *status == AdapterStatus::Stubbed && detail.contains("not ready")
            }
            _ => false,
        }));
        assert!(events.iter().all(|event| {
            !matches!(
                event,
                crate::AdapterEvent::ConversationUpsert { .. }
                    | crate::AdapterEvent::MessageReceived { .. }
            )
        }));
    }

    #[test]
    fn feature_on_emits_guild_inbox_placeholder_without_ready() {
        if !DiscordAdapter::bot_inbox_compiled() {
            return;
        }
        let mut adapter = DiscordAdapter::memory();
        let (tx, mut rx) = unbounded_channel();
        adapter
            .handle(
                AdapterCommand::ConnectDiscord {
                    mode: DiscordAuthMode::Bot,
                },
                &tx,
            )
            .expect("placeholder");
        let events = drain(&mut rx);
        let mut saw_guild = false;
        for event in &events {
            let rendered = format!("{event:?}");
            assert!(!rendered.contains("fixture"));
            match event {
                crate::AdapterEvent::Status { status, detail, .. } => {
                    assert_ne!(*status, AdapterStatus::Ready);
                    assert!(detail.contains("placeholder"));
                    assert!(detail.contains("not in the OS keychain"));
                }
                crate::AdapterEvent::ConversationUpsert { conversation } => {
                    assert!(conversation.id.contains("guild-inbox"));
                    assert!(!conversation.id.contains("dm"));
                    assert!(conversation.participant.contains("guild"));
                    saw_guild = true;
                }
                crate::AdapterEvent::MessageReceived { message } => {
                    assert!(message.body.contains("self-bot"));
                    assert!(!message.body.to_ascii_lowercase().contains("reliable"));
                }
                _ => {}
            }
        }
        assert!(saw_guild);
    }

    #[tokio::test]
    async fn stored_bot_token_is_not_logged_and_does_not_mark_ready() {
        if !DiscordAdapter::bot_inbox_compiled() {
            return;
        }
        let vault = Arc::new(MemoryDiscordVault::new());
        vault.set_bot_token("fixture-bot-token");
        let mut adapter = DiscordAdapter::new(Arc::clone(&vault) as Arc<dyn DiscordSecretVault>);
        let (tx, mut rx) = unbounded_channel();
        adapter
            .handle(
                AdapterCommand::ConnectDiscord {
                    mode: DiscordAuthMode::OAuth,
                },
                &tx,
            )
            .expect("armed placeholder");
        let events = drain(&mut rx);
        let rendered = format!("{events:?} {adapter:?}");
        assert!(!rendered.contains("fixture-bot-token"));
        assert!(events.iter().any(|event| matches!(
            event,
            crate::AdapterEvent::Status { status, detail, .. }
                if *status == AdapterStatus::Stubbed && detail.contains("in the OS keychain")
        )));
        assert!(events.iter().all(|event| {
            !matches!(
                event,
                crate::AdapterEvent::Status {
                    status: AdapterStatus::Ready,
                    ..
                }
            )
        }));
    }

    #[test]
    fn user_token_in_the_vault_is_refused_without_a_placeholder() {
        let vault = Arc::new(MemoryDiscordVault::new());
        vault.set_bot_token("User personal-token");
        let mut adapter = DiscordAdapter::new(Arc::clone(&vault) as Arc<dyn DiscordSecretVault>);
        let (tx, mut rx) = unbounded_channel();
        let err = adapter
            .handle(
                AdapterCommand::ConnectDiscord {
                    mode: DiscordAuthMode::Bot,
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
        assert!(rx.try_recv().is_err());
        let rendered = format!("{adapter:?}");
        assert!(!rendered.contains("personal-token"));
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

        let mut adapter = DiscordAdapter::memory();
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

    #[test]
    fn sources_do_not_automate_a_user_account() {
        let src = include_str!("mod.rs");
        assert!(!src.contains(concat!("Token::", "User")));
        assert!(!src.contains(concat!("self", "bot")));
        assert!(!src.contains(concat!("/users/", "@me")));
        assert!(!src.contains(concat!("seren", "ity")));
        let toml = include_str!("../../Cargo.toml");
        assert!(toml.contains("discord-bot"));
        assert!(toml.contains("default = []"));
        assert!(!toml.contains(concat!("seren", "ity")));
        let ci = include_str!("../../../../.github/workflows/ci.yml");
        let tests = include_str!("../../../../scripts/test.sh");
        let zips = include_str!("../../../../.github/workflows/os-zips.yml");
        assert!(!ci.contains("discord-bot"));
        assert!(!tests.contains("discord-bot"));
        assert!(!zips.contains("discord-bot"));
        let lock = include_str!("../../../../Cargo.lock");
        assert!(!lock.contains(concat!("seren", "ity")));
        assert!(lock.contains("name = \"twilight-http\""));
    }
}
