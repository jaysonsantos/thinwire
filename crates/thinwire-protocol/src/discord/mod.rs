//! Discord bot/OAuth guild inbox. User-account self-bots are refused.
//!
//! Feature `discord-bot` compiles the twilight bot HTTP backend. With a bot
//! token in the vault, the adapter lists guild channels that the bot can read,
//! loads recent history, and sends as the bot. Every HTTP call runs in a tokio
//! task on the adapter worker. No gateway is opened yet. Default builds stay
//! "not ready" and do not compile an HTTP client.

#[cfg(any(test, feature = "discord-bot"))]
mod api;
#[cfg(test)]
mod fake_api;
#[cfg(any(test, feature = "discord-bot"))]
mod inbox;
mod install;
#[cfg(any(test, feature = "discord-bot"))]
mod permissions;
#[cfg(any(test, feature = "discord-bot"))]
mod session;
mod token;
#[cfg(feature = "discord-bot")]
mod twilight;

use std::fmt;
use std::sync::Arc;
#[cfg(any(test, feature = "discord-bot"))]
use std::sync::atomic::{AtomicU64, Ordering};

use super::adapter::{
    AccountState, AdapterCommand, AdapterError, AdapterStatus, DiscordAuthMode, EventTx,
    ProtocolAdapter, ProtocolCapabilities, ProtocolId, SupportClass, emit_account, emit_status,
};
use token::authorization_token;

pub use install::DiscordOAuthInstall;
pub use token::{
    DISCORD_SECRET_BOT_TOKEN, DISCORD_SECRET_SERVICE, DiscordSecretVault, MemoryDiscordVault,
};

#[cfg(not(feature = "discord-bot"))]
const CAPABILITY_DETAIL: &str = "Bot/OAuth inbox only. Not ready in this build. Enable feature discord-bot. No user-account self-bots or personal DMs.";

#[cfg(feature = "discord-bot")]
const CAPABILITY_DETAIL: &str = "Bot/OAuth guild inbox (twilight HTTP). Lists guild channels the bot can read, loads recent history, and sends as the bot. No gateway yet. No user-account self-bots or personal DMs.";

const NOT_READY_DETAIL: &str = "Discord bot inbox is not ready in this build. Enable feature discord-bot. No user-account client is compiled.";

const NOT_READY_REASON: &str = "Discord bot inbox is not ready in this build";

#[cfg(any(test, feature = "discord-bot"))]
const NOT_CONNECTED_REASON: &str =
    "Discord bot inbox is not connected. Store a bot token, then connect";

const SELF_BOT_REFUSAL: &str = "Discord user-account / self-bot automation is refused. Bot/OAuth only. License-clean crates do not grant Discord permission to automate a personal account.";

const USER_TOKEN_REFUSAL: &str = "Discord user-account tokens are refused. A Bearer prefix does not prove application or bot provenance or the bot scope, so it is not sent. The keychain slot accepts a bot token.";

/// Refusal for a conversation id that is not in the last guild channel list.
#[cfg(any(test, feature = "discord-bot"))]
const UNKNOWN_CHANNEL_REFUSAL: &str = "That Discord conversation is not a guild channel the bot can read. Direct messages are out of scope.";

/// Detail fragment present only after a bot token was accepted.
const BOT_TOKEN_PRESENT: &str = "bot token is in the OS keychain";
/// Detail fragment for a feature-on connect that has no token yet.
#[cfg(any(test, feature = "discord-bot"))]
const BOT_TOKEN_MISSING: &str = "bot token is not in the OS keychain";

const CAPABILITIES: ProtocolCapabilities = ProtocolCapabilities {
    id: ProtocolId::Discord,
    support: SupportClass::Constrained,
    short_label: "Constrained · bot/OAuth inbox only",
    detail: CAPABILITY_DETAIL,
    official_api: true,
    allows_user_account_automation: false,
    sends_text: cfg!(feature = "discord-bot"),
};

/// Builds the bot HTTP backend from an accepted token. Runs on the tokio worker.
#[cfg(any(test, feature = "discord-bot"))]
type BackendFactory = Box<dyn Fn(String) -> Arc<dyn api::DiscordApi> + Send>;

/// Constrained Discord adapter. Never starts a user-account client.
pub struct DiscordAdapter {
    secrets: Arc<dyn DiscordSecretVault>,
    #[cfg(any(test, feature = "discord-bot"))]
    backend: Option<BackendFactory>,
    #[cfg(any(test, feature = "discord-bot"))]
    session: Option<session::Session>,
    /// Session generation. A bump makes running tasks drop their results.
    #[cfg(any(test, feature = "discord-bot"))]
    live: Arc<AtomicU64>,
    /// The chat `ViewChat` last named. `None` means the user left Discord.
    viewed: Option<String>,
}

impl DiscordAdapter {
    #[must_use]
    pub fn new(secrets: Arc<dyn DiscordSecretVault>) -> Self {
        Self {
            secrets,
            #[cfg(feature = "discord-bot")]
            backend: Some(Box::new(|token| {
                Arc::new(twilight::TwilightApi::new(token)) as Arc<dyn api::DiscordApi>
            })),
            #[cfg(all(test, not(feature = "discord-bot")))]
            backend: None,
            #[cfg(any(test, feature = "discord-bot"))]
            session: None,
            #[cfg(any(test, feature = "discord-bot"))]
            live: Arc::new(AtomicU64::new(0)),
            viewed: None,
        }
    }

    /// Adapter with an injected backend. Tests pass a fake here.
    #[cfg(test)]
    fn with_backend(secrets: Arc<dyn DiscordSecretVault>, backend: BackendFactory) -> Self {
        let mut adapter = Self::new(secrets);
        adapter.backend = Some(backend);
        adapter
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

    /// Linked-account marker for a guild inbox.
    ///
    /// A missing-token status is not linked. The bot token must be present
    /// and the adapter must not be refused or in error.
    #[must_use]
    pub fn inbox_account_linked(status: AdapterStatus, detail: &str) -> bool {
        Self::bot_inbox_compiled()
            && !matches!(status, AdapterStatus::Refused | AdapterStatus::Error)
            && detail.contains(BOT_TOKEN_PRESENT)
    }

    /// Explicit refusal used by tests and any future connect UI.
    pub fn connect_user_account() -> Result<(), AdapterError> {
        Err(AdapterError::Refused {
            protocol: ProtocolId::Discord,
            reason: SELF_BOT_REFUSAL,
        })
    }

    fn prepared_token(&self) -> Result<Option<String>, AdapterError> {
        let Some(raw) = self.secrets.bot_token() else {
            return Ok(None);
        };
        authorization_token(&raw)
            .map(Some)
            .map_err(|()| AdapterError::Refused {
                protocol: ProtocolId::Discord,
                reason: USER_TOKEN_REFUSAL,
            })
    }

    fn connect_bot_inbox(&mut self, events: &EventTx) -> Result<(), AdapterError> {
        #[cfg(any(test, feature = "discord-bot"))]
        let carried = self
            .session
            .as_ref()
            .map(session::Session::carried_channels)
            .unwrap_or_default();
        let history = self
            .session
            .as_ref()
            .map(session::Session::carried_history)
            .unwrap_or_default();
        let bodies = self
            .session
            .as_ref()
            .map(session::Session::carried_bodies)
            .unwrap_or_default();
        self.stop_session(events);
        let prepared = self.prepared_token()?;
        #[cfg(any(test, feature = "discord-bot"))]
        if let Some(factory) = &self.backend {
            match prepared {
                None => {
                    emit_status(
                        events,
                        ProtocolId::Discord,
                        AdapterStatus::Stubbed,
                        format!(
                            "Discord bot inbox. {BOT_TOKEN_MISSING}. Store a bot token to load guild channels. Not a personal Discord client."
                        ),
                    );
                    emit_account(events, ProtocolId::Discord, AccountState::Unlinked);
                }
                Some(token) => {
                    let api = factory(token);
                    self.session = Some(session::Session::start(
                        api, &self.live, events, carried, history, bodies,
                    ));
                }
            }
            return Ok(());
        }
        drop(prepared);
        emit_status(
            events,
            ProtocolId::Discord,
            AdapterStatus::Stubbed,
            NOT_READY_DETAIL,
        );
        emit_account(events, ProtocolId::Discord, AccountState::Unlinked);
        Ok(())
    }

    fn stop_session(&mut self, events: &EventTx) {
        // In-flight HTTP sends stay open. They settle when the request returns.
        let _ = events;
        #[cfg(any(test, feature = "discord-bot"))]
        {
            self.live.fetch_add(1, Ordering::SeqCst);
            self.session = None;
        }
    }

    #[cfg(any(test, feature = "discord-bot"))]
    fn session_mut(&mut self) -> Result<&mut session::Session, AdapterError> {
        if self.backend.is_none() {
            return Err(not_ready());
        }
        self.session.as_mut().ok_or(AdapterError::Unavailable {
            protocol: ProtocolId::Discord,
            reason: NOT_CONNECTED_REASON,
        })
    }

    #[cfg(any(test, feature = "discord-bot"))]
    fn load_chats(&mut self, events: &EventTx) -> Result<(), AdapterError> {
        self.session_mut()?.reload(events);
        Ok(())
    }

    #[cfg(any(test, feature = "discord-bot"))]
    fn open_chat(&mut self, conversation_id: String, events: &EventTx) -> Result<(), AdapterError> {
        self.session_mut()?.open(conversation_id, events)
    }

    #[cfg(any(test, feature = "discord-bot"))]
    fn send_text(
        &mut self,
        conversation_id: String,
        body: String,
        request: u64,
        events: &EventTx,
    ) -> Result<(), AdapterError> {
        self.session_mut()?
            .send(conversation_id, body, request, events)
    }

    #[cfg(any(test, feature = "discord-bot"))]
    fn resend(
        &mut self,
        conversation_id: String,
        message_id: String,
        request: u64,
        events: &EventTx,
    ) -> Result<(), AdapterError> {
        self.session_mut()?
            .resend(conversation_id, message_id, request, events)
    }

    #[cfg(not(any(test, feature = "discord-bot")))]
    fn load_chats(&mut self, _events: &EventTx) -> Result<(), AdapterError> {
        Err(not_ready())
    }

    #[cfg(not(any(test, feature = "discord-bot")))]
    fn open_chat(
        &mut self,
        _conversation_id: String,
        _events: &EventTx,
    ) -> Result<(), AdapterError> {
        Err(not_ready())
    }

    #[cfg(not(any(test, feature = "discord-bot")))]
    fn send_text(
        &mut self,
        _conversation_id: String,
        _body: String,
        _request: u64,
        _events: &EventTx,
    ) -> Result<(), AdapterError> {
        Err(not_ready())
    }

    #[cfg(not(any(test, feature = "discord-bot")))]
    fn resend(
        &mut self,
        _conversation_id: String,
        _message_id: String,
        _request: u64,
        _events: &EventTx,
    ) -> Result<(), AdapterError> {
        Err(not_ready())
    }
}

const fn not_ready() -> AdapterError {
    AdapterError::Unavailable {
        protocol: ProtocolId::Discord,
        reason: NOT_READY_REASON,
    }
}

impl fmt::Debug for DiscordAdapter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DiscordAdapter")
            .field("bot_inbox", &Self::bot_inbox_compiled())
            .field("token", &"<redacted>")
            .finish_non_exhaustive()
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
            emit_account(&events, ProtocolId::Discord, AccountState::Unlinked);
        }
    }

    /// Drop the bot session, then `Stopped`. Nothing is running until connect.
    fn shutdown(&mut self, events: &EventTx) {
        self.stop_session(events);
        super::adapter::emit_stopped(events, ProtocolId::Discord);
    }

    fn view_chat(&mut self, conversation_id: Option<&str>, events: &EventTx) {
        let _ = events;
        self.viewed = conversation_id.map(str::to_owned);
    }

    fn handle(&mut self, command: AdapterCommand, events: &EventTx) -> Result<(), AdapterError> {
        match command {
            AdapterCommand::ConnectDiscord {
                mode: DiscordAuthMode::UserAccount,
            } => {
                emit_account(events, ProtocolId::Discord, AccountState::Unlinked);
                Self::connect_user_account()
            }
            AdapterCommand::ConnectDiscord {
                mode: DiscordAuthMode::Bot | DiscordAuthMode::OAuth,
            }
            | AdapterCommand::Connect {
                protocol: ProtocolId::Discord,
            } => self.connect_bot_inbox(events),
            AdapterCommand::Disconnect {
                protocol: ProtocolId::Discord,
            } => {
                self.stop_session(events);
                let detail = if Self::bot_inbox_compiled() {
                    "Discord bot inbox disconnected."
                } else {
                    NOT_READY_DETAIL
                };
                emit_status(events, ProtocolId::Discord, AdapterStatus::Stubbed, detail);
                emit_account(events, ProtocolId::Discord, AccountState::Unlinked);
                Ok(())
            }
            AdapterCommand::LoadChats {
                protocol: ProtocolId::Discord,
            } => self.load_chats(events),
            AdapterCommand::OpenChat {
                protocol: ProtocolId::Discord,
                conversation_id,
            } => self.open_chat(conversation_id, events),
            AdapterCommand::SendText {
                protocol: ProtocolId::Discord,
                conversation_id,
                body,
                request,
            } => self.send_text(conversation_id, body, request, events),
            AdapterCommand::ResendMessage {
                protocol: ProtocolId::Discord,
                conversation_id,
                message_id,
                request,
            } => self.resend(conversation_id, message_id, request, events),
            _ => Err(AdapterError::Unavailable {
                protocol: ProtocolId::Discord,
                reason: "command is not handled by the Discord adapter",
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::fake_api::{
        BOT_ID, FakeDiscordApi, GENERAL, GUILD, LOCKED_GUILD, NEWS, SECRET, VOICE,
    };
    use super::inbox::conversation_id;
    use super::*;
    use crate::AdapterEvent;
    use tokio::sync::Notify;
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    const FIXTURE_TOKEN: &str = "fixture-bot-token";

    fn drain(rx: &mut UnboundedReceiver<AdapterEvent>) -> Vec<AdapterEvent> {
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    /// Collect events until `done` matches one. Fails after two seconds.
    async fn until(
        rx: &mut UnboundedReceiver<AdapterEvent>,
        done: impl Fn(&AdapterEvent) -> bool,
    ) -> Vec<AdapterEvent> {
        let mut events = Vec::new();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .expect("adapter event in time")
                .expect("event channel open");
            let stop = done(&event);
            events.push(event);
            if stop {
                return events;
            }
        }
    }

    fn is_ready(event: &AdapterEvent) -> bool {
        matches!(
            event,
            AdapterEvent::Status {
                status: AdapterStatus::Ready,
                ..
            }
        )
    }

    fn fake_adapter(
        api: Arc<FakeDiscordApi>,
        token: Option<&str>,
    ) -> (DiscordAdapter, Arc<MemoryDiscordVault>) {
        let vault = Arc::new(MemoryDiscordVault::new());
        if let Some(token) = token {
            vault.set_bot_token(token);
        }
        let adapter = DiscordAdapter::with_backend(
            Arc::clone(&vault) as Arc<dyn DiscordSecretVault>,
            Box::new(move |token| {
                assert_eq!(token, FIXTURE_TOKEN, "backend gets the vault token");
                Arc::clone(&api) as Arc<dyn api::DiscordApi>
            }),
        );
        (adapter, vault)
    }

    async fn inbox_loaded(rx: &mut UnboundedReceiver<AdapterEvent>) -> Vec<AdapterEvent> {
        let mut events = until(rx, is_ready).await;
        while let Ok(Some(event)) = tokio::time::timeout(Duration::from_millis(50), rx.recv()).await
        {
            events.push(event);
        }
        events
    }

    /// Connect with the guild fixture and wait for the channel list.
    async fn connected(
        api: Arc<FakeDiscordApi>,
    ) -> (
        DiscordAdapter,
        EventTx,
        UnboundedReceiver<AdapterEvent>,
        Vec<AdapterEvent>,
    ) {
        let (mut adapter, _vault) = fake_adapter(api, Some(FIXTURE_TOKEN));
        let (tx, mut rx) = unbounded_channel();
        adapter
            .handle(
                AdapterCommand::ConnectDiscord {
                    mode: DiscordAuthMode::Bot,
                },
                &tx,
            )
            .expect("connect");
        let events = inbox_loaded(&mut rx).await;
        (adapter, tx, rx, events)
    }

    fn conversations(events: &[AdapterEvent]) -> Vec<&crate::Conversation> {
        events
            .iter()
            .filter_map(|event| match event {
                AdapterEvent::ConversationUpsert { conversation } => Some(conversation),
                _ => None,
            })
            .collect()
    }

    fn messages(events: &[AdapterEvent]) -> Vec<&crate::ChatMessage> {
        events
            .iter()
            .filter_map(|event| match event {
                AdapterEvent::MessageReceived { message } => Some(message),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn capabilities_forbid_user_account_automation() {
        let caps = DiscordAdapter::capabilities();
        assert_eq!(caps.support, SupportClass::Constrained);
        assert!(!caps.allows_user_account_automation);
        assert_eq!(caps.sends_text, DiscordAdapter::bot_inbox_compiled());
        assert!(caps.short_label.contains("bot/OAuth"));
        assert!(!caps.detail.to_ascii_lowercase().contains("reliable"));
        assert!(caps.detail.contains("No user-account self-bots"));
        if DiscordAdapter::bot_inbox_compiled() {
            assert!(caps.detail.contains("guild channels the bot can read"));
            assert!(!caps.detail.to_ascii_lowercase().contains("not ready"));
        } else {
            assert!(caps.detail.to_ascii_lowercase().contains("not ready"));
        }
    }

    #[test]
    fn view_chat_remembers_the_channel_the_user_looks_at() {
        let (mut adapter, _vault) = fake_adapter(Arc::new(FakeDiscordApi::guild_fixture()), None);
        let (tx, _rx) = unbounded_channel();
        adapter.view_chat(Some("discord:1:2"), &tx);
        assert_eq!(adapter.viewed.as_deref(), Some("discord:1:2"));
        adapter.view_chat(None, &tx);
        assert!(adapter.viewed.is_none());
    }

    #[test]
    fn feature_off_connect_is_not_ready_without_conversations() {
        if DiscordAdapter::bot_inbox_compiled() {
            return;
        }
        let vault = Arc::new(MemoryDiscordVault::new());
        vault.set_bot_token(FIXTURE_TOKEN);
        let mut adapter = DiscordAdapter::new(vault);
        let (tx, mut rx) = unbounded_channel();
        adapter.start(tx.clone());
        let events = drain(&mut rx);
        assert!(events.iter().any(|event| matches!(
            event,
            AdapterEvent::Status { detail, status, .. }
                if *status == AdapterStatus::Stubbed && detail.contains("not ready")
        )));
        assert!(conversations(&events).is_empty());
        assert!(messages(&events).is_empty());
        for command in [
            AdapterCommand::LoadChats {
                protocol: ProtocolId::Discord,
            },
            AdapterCommand::OpenChat {
                protocol: ProtocolId::Discord,
                conversation_id: conversation_id(GUILD, GENERAL),
            },
        ] {
            assert!(matches!(
                adapter.handle(command, &tx),
                Err(AdapterError::Unavailable { .. })
            ));
        }
    }

    #[tokio::test]
    async fn missing_token_is_stubbed_and_not_linked() {
        let (mut adapter, _vault) = fake_adapter(Arc::new(FakeDiscordApi::guild_fixture()), None);
        let (tx, mut rx) = unbounded_channel();
        adapter.start(tx.clone());
        let events = drain(&mut rx);
        assert!(conversations(&events).is_empty());
        let Some(AdapterEvent::Status { status, detail, .. }) = events.first() else {
            panic!("missing-token status");
        };
        assert_eq!(*status, AdapterStatus::Stubbed);
        assert!(detail.contains(BOT_TOKEN_MISSING));
        assert!(!DiscordAdapter::inbox_account_linked(*status, detail));
        assert!(matches!(
            adapter.handle(
                AdapterCommand::LoadChats {
                    protocol: ProtocolId::Discord
                },
                &tx
            ),
            Err(AdapterError::Unavailable { .. })
        ));
    }

    #[tokio::test]
    async fn channel_list_shows_only_readable_guild_text_channels() {
        let (_adapter, _tx, _rx, events) =
            connected(Arc::new(FakeDiscordApi::guild_fixture())).await;
        let rendered = format!("{events:?}");
        assert!(!rendered.contains(FIXTURE_TOKEN));

        let rows = conversations(&events);
        let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                conversation_id(GUILD, GENERAL).as_str(),
                conversation_id(GUILD, NEWS).as_str(),
            ]
        );
        assert!(!rendered.contains(&conversation_id(GUILD, SECRET)));
        assert!(!rendered.contains(&conversation_id(GUILD, VOICE)));
        assert!(!rendered.contains(&LOCKED_GUILD.to_string()));
        let general = rows[0];
        assert_eq!(general.protocol, ProtocolId::Discord);
        assert_eq!(general.title, "#general");
        assert_eq!(general.participant, "Test guild");
        assert!(general.preview.contains("read and send"));
        assert!(general.writable);
        assert!(!rows[1].writable);
        assert!(rows[1].preview.contains("read only"));

        let ready_at = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    AdapterEvent::Status {
                        status: AdapterStatus::Ready,
                        detail,
                        ..
                    } if detail.contains("2 guild channels")
                )
            })
            .expect("ready status");
        let first_row = events
            .iter()
            .position(|event| matches!(event, AdapterEvent::ConversationUpsert { .. }))
            .expect("channel row");
        assert!(ready_at < first_row, "ready before the inbox");
        let linked_at = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    AdapterEvent::Account {
                        state: AccountState::Linked,
                        ..
                    }
                )
            })
            .expect("linked");
        assert!(linked_at < first_row, "linked before the inbox");
        let AdapterEvent::Status { status, detail, .. } = &events[ready_at] else {
            panic!("ready status");
        };
        assert!(
            DiscordAdapter::inbox_account_linked(*status, detail)
                == DiscordAdapter::bot_inbox_compiled()
        );
        assert!(events.iter().any(|event| matches!(
            event,
            AdapterEvent::Status {
                status: AdapterStatus::Connecting,
                ..
            }
        )));
    }

    #[tokio::test]
    async fn open_chat_loads_history_oldest_first() {
        let (mut adapter, tx, mut rx, _) =
            connected(Arc::new(FakeDiscordApi::guild_fixture())).await;
        let id = conversation_id(GUILD, GENERAL);
        adapter
            .handle(
                AdapterCommand::OpenChat {
                    protocol: ProtocolId::Discord,
                    conversation_id: id.clone(),
                },
                &tx,
            )
            .expect("open");
        let events = until(&mut rx, |event| {
            matches!(
                event,
                AdapterEvent::HistoryLoaded { conversation_id, .. } if conversation_id == &id
            )
        })
        .await;
        let rows = messages(&events);
        let bodies: Vec<&str> = rows.iter().map(|row| row.body.as_str()).collect();
        assert_eq!(
            bodies,
            vec!["hello guild", "[no text]", "reply from the bot"]
        );
        assert!(rows.iter().all(|row| row.conversation_id == id));
        assert_eq!(rows[0].sender, "alice");
        assert!(!rows[0].outbound);
        assert!(rows[2].outbound, "bot {BOT_ID} messages are outbound");
    }

    #[tokio::test]
    async fn reopen_drops_rows_missing_from_the_new_history_page() {
        let hold = Arc::new(Notify::new());
        let mut fake = FakeDiscordApi::guild_fixture();
        fake.hold_send = Some(Arc::clone(&hold));
        let api = Arc::new(fake);
        let (mut adapter, tx, mut rx, _) = connected(Arc::clone(&api)).await;
        let id = conversation_id(GUILD, GENERAL);
        adapter
            .handle(
                AdapterCommand::OpenChat {
                    protocol: ProtocolId::Discord,
                    conversation_id: id.clone(),
                },
                &tx,
            )
            .expect("open");
        let _ = until(&mut rx, |event| {
            matches!(event, AdapterEvent::HistoryLoaded { .. })
        })
        .await;
        api.state()
            .history
            .get_mut(&GENERAL)
            .expect("history")
            .retain(|message| message.id != 1);
        adapter
            .handle(
                AdapterCommand::SendText {
                    protocol: ProtocolId::Discord,
                    conversation_id: id.clone(),
                    body: "still sending".into(),
                    request: 3,
                },
                &tx,
            )
            .expect("pending");
        adapter
            .handle(
                AdapterCommand::OpenChat {
                    protocol: ProtocolId::Discord,
                    conversation_id: id.clone(),
                },
                &tx,
            )
            .expect("reopen");
        let events = until(&mut rx, |event| {
            matches!(event, AdapterEvent::HistoryLoaded { .. })
        })
        .await;
        let removed: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                AdapterEvent::MessagesRemoved { message_ids, .. } => {
                    Some(message_ids.iter().map(String::as_str))
                }
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(removed, vec!["discord:1"]);
        hold.notify_waiters();
    }

    #[tokio::test]
    async fn history_failure_is_a_channel_note_and_not_ready() {
        let api = Arc::new(FakeDiscordApi::guild_fixture());
        let (mut adapter, tx, mut rx, _) = connected(Arc::clone(&api)).await;
        api.state().next_error = Some(api::DiscordApiError::Transport);
        let id = conversation_id(GUILD, GENERAL);
        adapter
            .handle(
                AdapterCommand::OpenChat {
                    protocol: ProtocolId::Discord,
                    conversation_id: id.clone(),
                },
                &tx,
            )
            .expect("open");
        let events = until(&mut rx, |event| {
            matches!(event, AdapterEvent::HistoryLoaded { .. })
        })
        .await;
        assert!(events.iter().any(|event| matches!(
            event,
            AdapterEvent::CommandFailed { detail, conversation_id, .. }
                if conversation_id.as_deref() == Some(id.as_str())
                    && detail.contains("network error")
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            AdapterEvent::HistoryLoaded { conversation_id, .. } if conversation_id == &id
        )));
        assert!(!events.iter().any(|event| matches!(
            event,
            AdapterEvent::Status {
                status: AdapterStatus::Ready,
                ..
            }
        )));
        assert!(messages(&events).is_empty());
    }

    #[tokio::test]
    async fn send_text_shows_pending_then_the_sent_message() {
        let api = Arc::new(FakeDiscordApi::guild_fixture());
        let (mut adapter, tx, mut rx, _) = connected(Arc::clone(&api)).await;
        let id = conversation_id(GUILD, GENERAL);
        adapter
            .handle(
                AdapterCommand::SendText {
                    protocol: ProtocolId::Discord,
                    conversation_id: id.clone(),
                    body: "hi from thinwire".into(),
                    request: 0,
                },
                &tx,
            )
            .expect("send");
        let events = until(&mut rx, |event| {
            matches!(event, AdapterEvent::MessageReplaced { .. })
        })
        .await;
        assert!(events.iter().any(|event| matches!(
            event,
            AdapterEvent::SendAccepted { request: 0, conversation_id, .. }
                if conversation_id == &id
        )));
        let pending = messages(&events);
        assert_eq!(pending.len(), 1);
        assert!(pending[0].outbound);
        assert!(pending[0].id.starts_with("discord:pending:"));
        let Some(AdapterEvent::MessageReplaced {
            protocol,
            conversation_id,
            old_id,
            message,
        }) = events.last()
        else {
            panic!("replace event");
        };
        assert_eq!(*protocol, ProtocolId::Discord);
        assert_eq!(conversation_id, &id);
        assert_eq!(old_id, &pending[0].id);
        assert_eq!(message.body, "hi from thinwire");
        assert!(message.outbound);
        assert_eq!(
            api.state().sent,
            vec![(GENERAL, "hi from thinwire".to_string())]
        );
    }

    #[tokio::test]
    async fn pending_send_uses_the_local_clock_so_it_sorts_after_history() {
        let hold = Arc::new(Notify::new());
        let mut fake = FakeDiscordApi::guild_fixture();
        fake.hold_send = Some(Arc::clone(&hold));
        let (mut adapter, tx, mut rx, _) = connected(Arc::new(fake)).await;
        let id = conversation_id(GUILD, GENERAL);
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_secs() as i64;
        adapter
            .handle(
                AdapterCommand::SendText {
                    protocol: ProtocolId::Discord,
                    conversation_id: id,
                    body: "still sending".into(),
                    request: 1,
                },
                &tx,
            )
            .expect("pending");
        let events = until(&mut rx, |event| {
            matches!(event, AdapterEvent::MessageReceived { .. })
        })
        .await;
        let pending = messages(&events);
        assert_eq!(pending.len(), 1);
        let after = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_secs() as i64;
        assert!(
            pending[0].sent_at >= before && pending[0].sent_at <= after,
            "pending sent_at {} is the local clock",
            pending[0].sent_at
        );
        let history_at = inbox::snowflake_unix_seconds(3);
        assert!(
            pending[0].sent_at > history_at,
            "pending row sorts after channel history"
        );
        hold.notify_waiters();
    }

    #[tokio::test]
    async fn a_new_session_rejects_inflight_sends() {
        let hold = Arc::new(Notify::new());
        let mut fake = FakeDiscordApi::guild_fixture();
        fake.hold_send = Some(Arc::clone(&hold));
        let (mut adapter, tx, mut rx, _) = connected(Arc::new(fake)).await;
        let id = conversation_id(GUILD, GENERAL);
        for request in [4_u64, 9] {
            adapter
                .handle(
                    AdapterCommand::SendText {
                        protocol: ProtocolId::Discord,
                        conversation_id: id.clone(),
                        body: format!("queued {request}"),
                        request,
                    },
                    &tx,
                )
                .expect("queued");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        adapter
            .handle(
                AdapterCommand::Connect {
                    protocol: ProtocolId::Discord,
                },
                &tx,
            )
            .expect("reconnect");
        tokio::time::sleep(Duration::from_millis(30)).await;
        let early = drain(&mut rx);
        assert!(
            !early.iter().any(|event| matches!(
                event,
                AdapterEvent::SendRejected { .. } | AdapterEvent::MessagesRemoved { .. }
            )),
            "a live HTTP send stays pending until Discord answers"
        );
        hold.notify_waiters();
        let events = until(&mut rx, |event| {
            matches!(event, AdapterEvent::SendRejected { request: 9, .. })
        })
        .await;
        let rejected: Vec<u64> = events
            .iter()
            .filter_map(|event| match event {
                AdapterEvent::SendRejected { request, .. } => Some(*request),
                _ => None,
            })
            .collect();
        assert_eq!(rejected, vec![4, 9]);
        let pending: Vec<&str> = early
            .iter()
            .filter_map(|event| match event {
                AdapterEvent::MessageReceived { message } if message.id.contains(":pending:") => {
                    Some(message.id.as_str())
                }
                _ => None,
            })
            .collect();
        let removed: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                AdapterEvent::MessagesRemoved { message_ids, .. } => {
                    Some(message_ids.iter().map(String::as_str))
                }
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(pending, vec!["discord:pending:2:1", "discord:pending:2:2"]);
        assert_eq!(removed, pending);
        assert!(!events.iter().any(|event| matches!(
            event,
            AdapterEvent::SendAccepted { .. } | AdapterEvent::MessageReplaced { .. }
        )));
        adapter
            .handle(
                AdapterCommand::SendText {
                    protocol: ProtocolId::Discord,
                    conversation_id: id,
                    body: "after reconnect".into(),
                    request: 11,
                },
                &tx,
            )
            .expect("new pending");
        let later = until(&mut rx, |event| {
            matches!(
                event,
                AdapterEvent::MessageReceived { message, .. } if message.id.contains(":pending:")
            )
        })
        .await;
        let new_id = messages(&later)[0].id.as_str();
        assert!(
            !pending.contains(&new_id),
            "a replacement session does not reuse a pending id"
        );
        hold.notify_waiters();
    }

    #[tokio::test]
    async fn failed_send_marks_the_row_and_a_retry_posts_it_again() {
        let api = Arc::new(FakeDiscordApi::guild_fixture());
        api.state().send_error = Some(api::DiscordApiError::Forbidden);
        let (mut adapter, tx, mut rx, _) = connected(Arc::clone(&api)).await;
        let conversation_id = conversation_id(GUILD, GENERAL);
        adapter
            .handle(
                AdapterCommand::SendText {
                    protocol: ProtocolId::Discord,
                    conversation_id: conversation_id.clone(),
                    body: "blocked".into(),
                    request: 0,
                },
                &tx,
            )
            .expect("queued");
        let events = until(&mut rx, is_ready).await;
        let pending = messages(&events)[0].id.clone();
        assert!(events.iter().any(|event| matches!(
            event,
            AdapterEvent::MessageDelivery {
                message_id,
                delivery: crate::Delivery::Failed,
                ..
            } if message_id == &pending
        )));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AdapterEvent::SendRejected { request: 0, .. }))
        );
        api.state().send_error = None;
        adapter
            .handle(
                AdapterCommand::ResendMessage {
                    protocol: ProtocolId::Discord,
                    conversation_id: conversation_id.clone(),
                    message_id: pending.clone(),
                    request: 3,
                },
                &tx,
            )
            .expect("retry");
        let retried = until(&mut rx, |event| {
            matches!(event, AdapterEvent::MessageReplaced { .. })
        })
        .await;
        assert!(
            retried
                .iter()
                .any(|event| matches!(event, AdapterEvent::SendAccepted { request: 3, .. }))
        );
        assert!(retried.iter().any(|event| matches!(
            event,
            AdapterEvent::MessageReplaced { old_id, message, .. }
                if old_id == &pending && message.body == "blocked"
        )));
        assert_eq!(api.state().sent, vec![(GENERAL, "blocked".to_string())]);
    }

    #[tokio::test]
    async fn retry_after_reconnect_posts_the_failed_body() {
        let api = Arc::new(FakeDiscordApi::guild_fixture());
        api.state().send_error = Some(api::DiscordApiError::Forbidden);
        let (mut adapter, tx, mut rx, _) = connected(Arc::clone(&api)).await;
        let id = conversation_id(GUILD, GENERAL);
        adapter
            .handle(
                AdapterCommand::SendText {
                    protocol: ProtocolId::Discord,
                    conversation_id: id.clone(),
                    body: "try later".into(),
                    request: 1,
                },
                &tx,
            )
            .expect("queued");
        let events = until(&mut rx, is_ready).await;
        let pending = messages(&events)[0].id.clone();
        api.state().send_error = None;
        adapter
            .handle(
                AdapterCommand::Connect {
                    protocol: ProtocolId::Discord,
                },
                &tx,
            )
            .expect("reconnect");
        let _ = inbox_loaded(&mut rx).await;
        adapter
            .handle(
                AdapterCommand::ResendMessage {
                    protocol: ProtocolId::Discord,
                    conversation_id: id,
                    message_id: pending,
                    request: 4,
                },
                &tx,
            )
            .expect("retry");
        let retried = until(&mut rx, |event| {
            matches!(event, AdapterEvent::MessageReplaced { .. })
        })
        .await;
        assert!(
            retried
                .iter()
                .any(|event| matches!(event, AdapterEvent::SendAccepted { request: 4, .. }))
        );
        assert_eq!(api.state().sent, vec![(GENERAL, "try later".to_string())]);
    }

    #[tokio::test]
    async fn read_only_and_unknown_channels_refuse_send() {
        let api = Arc::new(FakeDiscordApi::guild_fixture());
        let (mut adapter, tx, mut rx, _) = connected(Arc::clone(&api)).await;
        for id in [
            conversation_id(GUILD, NEWS),
            conversation_id(GUILD, SECRET),
            // Looks like a DM channel id. It is not in the guild list.
            "discord:0:777".to_string(),
        ] {
            adapter
                .handle(
                    AdapterCommand::SendText {
                        protocol: ProtocolId::Discord,
                        conversation_id: id,
                        body: "nope".into(),
                        request: 0,
                    },
                    &tx,
                )
                .expect("refusal is a note");
        }
        adapter
            .handle(
                AdapterCommand::OpenChat {
                    protocol: ProtocolId::Discord,
                    conversation_id: conversation_id(GUILD, SECRET),
                },
                &tx,
            )
            .expect("unknown channel is a note");
        let events = drain(&mut rx);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AdapterEvent::SendRejected { .. }))
                .count(),
            3,
            "each refused send names itself so the draft can stay"
        );
        assert!(events.iter().any(|event| matches!(
            event,
            AdapterEvent::Notice { text, .. } if text.contains("Send Messages")
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            AdapterEvent::Notice { text, .. } if text.contains("Direct messages are out of scope")
        )));
        assert!(!events.iter().any(|event| matches!(
            event,
            AdapterEvent::Status {
                status: AdapterStatus::Refused,
                ..
            }
        )));
        assert!(api.state().sent.is_empty());
    }

    #[tokio::test]
    async fn an_older_channel_list_does_not_overwrite_a_newer_one() {
        let hold = Arc::new(Notify::new());
        let api = Arc::new(FakeDiscordApi::guild_fixture());
        let (mut adapter, tx, mut rx, _) = connected(Arc::clone(&api)).await;
        api.state().hold_channels = Some(Arc::clone(&hold));
        adapter
            .handle(
                AdapterCommand::LoadChats {
                    protocol: ProtocolId::Discord,
                },
                &tx,
            )
            .expect("slow reload");
        tokio::time::sleep(Duration::from_millis(30)).await;
        api.state()
            .channels
            .get_mut(&GUILD)
            .expect("guild")
            .retain(|channel| channel.id != NEWS);
        api.state().hold_channels = None;
        adapter
            .handle(
                AdapterCommand::LoadChats {
                    protocol: ProtocolId::Discord,
                },
                &tx,
            )
            .expect("fast reload");
        let events = inbox_loaded(&mut rx).await;
        assert!(events.iter().any(|event| matches!(
            event,
            AdapterEvent::ConversationRemoved { id, .. } if *id == conversation_id(GUILD, NEWS)
        )));
        hold.notify_waiters();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let late = drain(&mut rx);
        assert!(
            !conversations(&late)
                .iter()
                .any(|row| row.id == conversation_id(GUILD, NEWS)),
            "the slower list must not put the channel back"
        );
    }

    #[tokio::test]
    async fn reload_removes_channels_that_left_the_list() {
        let api = Arc::new(FakeDiscordApi::guild_fixture());
        let (mut adapter, tx, mut rx, _) = connected(Arc::clone(&api)).await;
        api.state()
            .channels
            .get_mut(&GUILD)
            .expect("guild")
            .retain(|channel| channel.id != NEWS);
        adapter
            .handle(
                AdapterCommand::LoadChats {
                    protocol: ProtocolId::Discord,
                },
                &tx,
            )
            .expect("reload");
        let events = inbox_loaded(&mut rx).await;
        assert!(events.iter().any(|event| matches!(
            event,
            AdapterEvent::ConversationRemoved { id, .. } if *id == conversation_id(GUILD, NEWS)
        )));
        assert_eq!(conversations(&events).len(), 1);
        let ready_at = events.iter().position(is_ready).expect("ready");
        let row_at = events
            .iter()
            .position(|event| matches!(event, AdapterEvent::ConversationUpsert { .. }))
            .expect("remaining channel");
        assert!(ready_at < row_at);
    }

    #[tokio::test]
    async fn reconnect_blocks_open_until_the_bot_id_returns() {
        let api = Arc::new(FakeDiscordApi::guild_fixture());
        let (mut adapter, tx, mut rx, _) = connected(Arc::clone(&api)).await;
        let hold = Arc::new(Notify::new());
        api.state().hold_load = Some(Arc::clone(&hold));
        adapter
            .handle(
                AdapterCommand::Connect {
                    protocol: ProtocolId::Discord,
                },
                &tx,
            )
            .expect("reconnect");
        tokio::time::sleep(Duration::from_millis(20)).await;
        let opened = adapter.handle(
            AdapterCommand::OpenChat {
                protocol: ProtocolId::Discord,
                conversation_id: conversation_id(GUILD, GENERAL),
            },
            &tx,
        );
        assert!(matches!(
            opened,
            Err(AdapterError::Unavailable { reason, .. }) if reason.contains("still loading")
        ));
        let events = drain(&mut rx);
        assert!(messages(&events).is_empty());
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AdapterEvent::Notice { .. }))
        );
        hold.notify_waiters();
    }

    #[tokio::test]
    async fn reconnect_drops_rows_the_new_page_no_longer_has() {
        let api = Arc::new(FakeDiscordApi::guild_fixture());
        let (mut adapter, tx, mut rx, _) = connected(Arc::clone(&api)).await;
        let id = conversation_id(GUILD, GENERAL);
        adapter
            .handle(
                AdapterCommand::OpenChat {
                    protocol: ProtocolId::Discord,
                    conversation_id: id.clone(),
                },
                &tx,
            )
            .expect("open");
        let _ = until(&mut rx, |event| {
            matches!(event, AdapterEvent::HistoryLoaded { .. })
        })
        .await;
        adapter
            .handle(
                AdapterCommand::SendText {
                    protocol: ProtocolId::Discord,
                    conversation_id: id.clone(),
                    body: "posted".into(),
                    request: 1,
                },
                &tx,
            )
            .expect("send");
        let sent = until(&mut rx, |event| {
            matches!(event, AdapterEvent::MessageReplaced { .. })
        })
        .await;
        let sent_id = sent
            .iter()
            .find_map(|event| match event {
                AdapterEvent::MessageReplaced { message, .. } => Some(message.id.clone()),
                _ => None,
            })
            .expect("sent id");
        adapter
            .handle(
                AdapterCommand::Connect {
                    protocol: ProtocolId::Discord,
                },
                &tx,
            )
            .expect("reconnect");
        let _ = inbox_loaded(&mut rx).await;
        api.state()
            .history
            .get_mut(&GENERAL)
            .expect("history")
            .retain(|message| message.id != 1);
        adapter
            .handle(
                AdapterCommand::OpenChat {
                    protocol: ProtocolId::Discord,
                    conversation_id: id,
                },
                &tx,
            )
            .expect("reopen");
        let events = until(&mut rx, |event| {
            matches!(event, AdapterEvent::HistoryLoaded { .. })
        })
        .await;
        let removed: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                AdapterEvent::MessagesRemoved { message_ids, .. } => {
                    Some(message_ids.iter().map(String::as_str))
                }
                _ => None,
            })
            .flatten()
            .collect();
        assert!(removed.contains(&"discord:1"), "aged-out history row");
        assert!(
            removed.iter().any(|row| *row == sent_id),
            "a sent row is part of the page the next load replaces"
        );
    }

    #[tokio::test]
    async fn connect_removes_channels_that_left_the_list() {
        let api = Arc::new(FakeDiscordApi::guild_fixture());
        let (mut adapter, tx, mut rx, _) = connected(Arc::clone(&api)).await;
        api.state()
            .channels
            .get_mut(&GUILD)
            .expect("guild")
            .retain(|channel| channel.id != NEWS);
        adapter
            .handle(
                AdapterCommand::Connect {
                    protocol: ProtocolId::Discord,
                },
                &tx,
            )
            .expect("reconnect");
        let events = inbox_loaded(&mut rx).await;
        assert!(events.iter().any(|event| matches!(
            event,
            AdapterEvent::ConversationRemoved { id, .. } if *id == conversation_id(GUILD, NEWS)
        )));
        assert_eq!(conversations(&events).len(), 1);
    }

    /// Channel list fails with `error`. No conversations. The token stays out of the detail.
    async fn channel_list_fails(error: api::DiscordApiError, fragment: &str) {
        let api = Arc::new(FakeDiscordApi::guild_fixture());
        api.state().next_error = Some(error);
        let (mut adapter, _vault) = fake_adapter(api, Some(FIXTURE_TOKEN));
        let (tx, mut rx) = unbounded_channel();
        adapter.start(tx);
        let events = until(&mut rx, |event| {
            matches!(
                event,
                AdapterEvent::Status {
                    status: AdapterStatus::Error,
                    ..
                }
            )
        })
        .await;
        assert!(conversations(&events).is_empty());
        assert!(!format!("{events:?}").contains(FIXTURE_TOKEN));
        let Some(AdapterEvent::Status { status, detail, .. }) = events.last() else {
            panic!("error status");
        };
        assert_eq!(*status, AdapterStatus::Error);
        assert!(detail.contains("did not load"));
        assert!(detail.contains(fragment));
        assert!(!DiscordAdapter::inbox_account_linked(*status, detail));
    }

    #[tokio::test]
    async fn network_error_stops_the_channel_list() {
        channel_list_fails(api::DiscordApiError::Transport, "network error").await;
    }

    #[tokio::test]
    async fn rate_limit_stops_the_channel_list() {
        channel_list_fails(api::DiscordApiError::RateLimited, "rate limit").await;
    }

    #[tokio::test]
    async fn revoked_token_stops_a_later_channel_list() {
        let api = Arc::new(FakeDiscordApi::guild_fixture());
        let (mut adapter, tx, mut rx, events) = connected(Arc::clone(&api)).await;
        assert_eq!(conversations(&events).len(), 2);
        api.state().unauthorized = true;
        adapter
            .handle(
                AdapterCommand::LoadChats {
                    protocol: ProtocolId::Discord,
                },
                &tx,
            )
            .expect("reload");
        let events = until(&mut rx, |event| {
            matches!(
                event,
                AdapterEvent::Status {
                    status: AdapterStatus::Error,
                    ..
                }
            )
        })
        .await;
        assert!(conversations(&events).is_empty());
        assert!(!format!("{events:?}").contains(FIXTURE_TOKEN));
        let Some(AdapterEvent::Status { status, detail, .. }) = events.last() else {
            panic!("error status");
        };
        assert!(detail.contains("did not load"));
        assert!(detail.contains("Replace discord.bot_token"));
        assert!(!DiscordAdapter::inbox_account_linked(*status, detail));
    }

    #[tokio::test]
    async fn rejected_token_reports_an_error_without_conversations() {
        let api = Arc::new(FakeDiscordApi::guild_fixture());
        api.state().unauthorized = true;
        let (mut adapter, _vault) = fake_adapter(api, Some(FIXTURE_TOKEN));
        let (tx, mut rx) = unbounded_channel();
        adapter.start(tx);
        let events = until(&mut rx, |event| {
            matches!(
                event,
                AdapterEvent::Status {
                    status: AdapterStatus::Error,
                    ..
                }
            )
        })
        .await;
        assert!(conversations(&events).is_empty());
        let Some(AdapterEvent::Status { status, detail, .. }) = events.last() else {
            panic!("error status");
        };
        assert!(detail.contains("Replace discord.bot_token"));
        assert!(!detail.contains(FIXTURE_TOKEN));
        assert!(!DiscordAdapter::inbox_account_linked(*status, detail));
    }

    #[tokio::test]
    async fn a_replaced_history_load_still_finishes() {
        let hold = Arc::new(Notify::new());
        let mut fake = FakeDiscordApi::guild_fixture();
        fake.hold_history = Some(Arc::clone(&hold));
        let (mut adapter, tx, mut rx, _) = connected(Arc::new(fake)).await;
        let id = conversation_id(GUILD, GENERAL);
        adapter
            .handle(
                AdapterCommand::OpenChat {
                    protocol: ProtocolId::Discord,
                    conversation_id: id.clone(),
                },
                &tx,
            )
            .expect("open");
        tokio::time::sleep(Duration::from_millis(20)).await;
        adapter
            .handle(
                AdapterCommand::Connect {
                    protocol: ProtocolId::Discord,
                },
                &tx,
            )
            .expect("reconnect");
        hold.notify_waiters();
        let events = until(&mut rx, |event| {
            matches!(
                event,
                AdapterEvent::HistoryLoaded { conversation_id, .. } if conversation_id == &id
            )
        })
        .await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let rest = drain(&mut rx);
        let loaded = events
            .iter()
            .chain(rest.iter())
            .filter(|event| {
                matches!(
                    event,
                    AdapterEvent::HistoryLoaded { conversation_id, .. } if conversation_id == &id
                )
            })
            .count();
        assert_eq!(loaded, 1, "the replaced open ends once");
        assert!(
            messages(&events).is_empty() && messages(&rest).is_empty(),
            "a replaced load does not publish rows"
        );
    }

    #[tokio::test]
    async fn disconnect_drops_history_from_the_old_session() {
        let hold = Arc::new(Notify::new());
        let mut fake = FakeDiscordApi::guild_fixture();
        fake.hold_history = Some(Arc::clone(&hold));
        let (mut adapter, tx, mut rx, _) = connected(Arc::new(fake)).await;
        adapter
            .handle(
                AdapterCommand::OpenChat {
                    protocol: ProtocolId::Discord,
                    conversation_id: conversation_id(GUILD, GENERAL),
                },
                &tx,
            )
            .expect("open");
        adapter
            .handle(
                AdapterCommand::Disconnect {
                    protocol: ProtocolId::Discord,
                },
                &tx,
            )
            .expect("disconnect");
        hold.notify_one();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let events = drain(&mut rx);
        assert!(messages(&events).is_empty());
        assert!(matches!(
            adapter.handle(
                AdapterCommand::OpenChat {
                    protocol: ProtocolId::Discord,
                    conversation_id: conversation_id(GUILD, GENERAL),
                },
                &tx
            ),
            Err(AdapterError::Unavailable { .. })
        ));
    }

    #[tokio::test]
    async fn connect_after_hydrate_loads_channels_that_start_missed() {
        let (mut adapter, vault) = fake_adapter(Arc::new(FakeDiscordApi::guild_fixture()), None);
        let (tx, mut rx) = unbounded_channel();
        adapter.start(tx.clone());
        assert!(drain(&mut rx).iter().any(|event| matches!(
            event,
            AdapterEvent::Status { detail, .. } if detail.contains(BOT_TOKEN_MISSING)
        )));
        vault.set_bot_token(FIXTURE_TOKEN);
        adapter
            .handle(
                AdapterCommand::Connect {
                    protocol: ProtocolId::Discord,
                },
                &tx,
            )
            .expect("reconnect");
        let events = inbox_loaded(&mut rx).await;
        assert_eq!(conversations(&events).len(), 2);
        let rendered = format!("{events:?} {adapter:?}");
        assert!(!rendered.contains(FIXTURE_TOKEN));
    }

    #[test]
    fn missing_token_detail_does_not_mark_the_account_linked() {
        assert!(
            !BOT_TOKEN_MISSING.contains(BOT_TOKEN_PRESENT),
            "the missing-token sentence must not satisfy the armed check"
        );
        let missing = format!("Discord bot inbox. {BOT_TOKEN_MISSING}.");
        assert!(!DiscordAdapter::inbox_account_linked(
            AdapterStatus::Stubbed,
            &missing
        ));
        let armed = format!("Discord bot inbox. {BOT_TOKEN_PRESENT}.");
        assert_eq!(
            DiscordAdapter::inbox_account_linked(AdapterStatus::Ready, &armed),
            DiscordAdapter::bot_inbox_compiled()
        );
        assert!(!DiscordAdapter::inbox_account_linked(
            AdapterStatus::Refused,
            &armed
        ));
    }

    #[test]
    fn unverified_bearer_in_the_vault_is_refused_without_events() {
        let vault = Arc::new(MemoryDiscordVault::new());
        vault.set_bot_token("Bearer oauth-fixture");
        let mut adapter = DiscordAdapter::new(Arc::clone(&vault) as Arc<dyn DiscordSecretVault>);
        let (tx, mut rx) = unbounded_channel();
        let err = adapter
            .handle(
                AdapterCommand::ConnectDiscord {
                    mode: DiscordAuthMode::OAuth,
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
        assert!(!rendered.contains("oauth-fixture"));
        assert!(!rendered.to_ascii_lowercase().contains("bearer oauth"));
    }

    #[test]
    fn user_token_in_the_vault_is_refused_without_events() {
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
        assert!(matches!(
            rx.try_recv(),
            Ok(AdapterEvent::Account {
                protocol: ProtocolId::Discord,
                state: AccountState::Unlinked,
            })
        ));
        assert!(rx.try_recv().is_err(), "refusal does not start a session");
    }

    #[test]
    fn sources_do_not_automate_a_user_account() {
        for src in [
            include_str!("mod.rs"),
            include_str!("api.rs"),
            include_str!("inbox.rs"),
            include_str!("session.rs"),
            include_str!("twilight.rs"),
        ] {
            assert!(!src.contains(concat!("Token::", "User")));
            assert!(!src.contains(concat!("self", "bot")));
            assert!(!src.contains(concat!("/users/", "@me")));
            assert!(!src.contains(concat!("seren", "ity")));
            assert!(!src.contains(concat!("private_", "channels")));
            assert!(!src.contains(concat!("create_", "private_channel")));
        }
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
