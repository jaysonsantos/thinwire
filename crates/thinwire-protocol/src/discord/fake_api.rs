//! In-memory [`DiscordApi`] for adapter tests. No network.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

use super::api::{
    ApiFuture, ChannelKind, ChannelSummary, DiscordApi, DiscordApiError, GuildSummary,
    MessageSummary, Overwrite, OverwriteTarget, SendResultPause,
};
use super::permissions::{READ_BITS, SEND_MESSAGES, VIEW_CHANNEL};

pub(crate) const BOT_ID: u64 = 9_000;
pub(crate) const GUILD: u64 = 1_000;
pub(crate) const LOCKED_GUILD: u64 = 2_000;
pub(crate) const GENERAL: u64 = 1_001;
pub(crate) const NEWS: u64 = 1_002;
pub(crate) const SECRET: u64 = 1_003;
pub(crate) const VOICE: u64 = 1_004;
pub(crate) const READONLY_ROLE: u64 = 1_500;

#[derive(Debug, Default)]
pub(crate) struct FakeState {
    pub guilds: Vec<GuildSummary>,
    pub roles: HashMap<u64, Vec<u64>>,
    pub channels: HashMap<u64, Vec<ChannelSummary>>,
    pub history: HashMap<u64, Vec<MessageSummary>>,
    pub sent: Vec<(u64, String)>,
    /// Every call returns 401 until this is cleared. A revoked bot token.
    pub unauthorized: bool,
    pub send_error: Option<DiscordApiError>,
    /// The next call returns this error once, then clears it.
    pub next_error: Option<DiscordApiError>,
    /// Pauses the next bot-id read until notified. A reconnect stays without a bot id.
    pub hold_load: Option<Arc<Notify>>,
    /// Pauses `channels` after the list is copied, so an older reload can finish late.
    pub hold_channels: Option<Arc<Notify>>,
    /// Fired when `channels` is about to wait on `hold_channels`.
    pub channels_at_barrier: Option<Arc<Notify>>,
    pub next_id: u64,
}

/// Fake bot HTTP backend. `hold_history` pauses history until notified.
#[derive(Debug, Default)]
pub(crate) struct FakeDiscordApi {
    pub state: Mutex<FakeState>,
    pub hold_history: Option<Arc<Notify>>,
    pub hold_send: Option<Arc<Notify>>,
    /// Sends currently blocked in `hold_send`.
    pub sends_at_hold: AtomicUsize,
    /// When set, a finished send waits after its result event is queued.
    pub send_result_pause: Option<Arc<SendResultPause>>,
}

fn channel(id: u64, name: &str, kind: ChannelKind, overwrites: Vec<Overwrite>) -> ChannelSummary {
    ChannelSummary {
        id,
        name: name.into(),
        kind,
        last_message_id: Some(id * 10),
        overwrites,
    }
}

fn message(id: u64, author_id: u64, author: &str, content: &str) -> MessageSummary {
    MessageSummary {
        id,
        author_id,
        author: author.into(),
        content: content.into(),
        attachments: 0,
    }
}

impl FakeDiscordApi {
    /// One readable guild and one guild that hides the bot member.
    ///
    /// `#general` is read and send. `#news` is read only through a role deny.
    /// `#secret` hides from `@everyone`. The voice channel is not a text channel.
    pub(crate) fn guild_fixture() -> Self {
        let mut state = FakeState {
            next_id: 50_000,
            ..FakeState::default()
        };
        state.guilds = vec![
            GuildSummary {
                id: GUILD,
                name: "Test guild".into(),
                owner: false,
                permissions: READ_BITS | SEND_MESSAGES,
            },
            GuildSummary {
                id: LOCKED_GUILD,
                name: "Locked guild".into(),
                owner: false,
                permissions: READ_BITS,
            },
        ];
        state.roles.insert(GUILD, vec![READONLY_ROLE]);
        state.channels.insert(
            GUILD,
            vec![
                channel(GENERAL, "general", ChannelKind::Text, Vec::new()),
                channel(
                    NEWS,
                    "news",
                    ChannelKind::Announcement,
                    vec![Overwrite {
                        target: OverwriteTarget::Role(READONLY_ROLE),
                        allow: 0,
                        deny: SEND_MESSAGES,
                    }],
                ),
                channel(
                    SECRET,
                    "secret",
                    ChannelKind::Text,
                    vec![Overwrite {
                        target: OverwriteTarget::Role(GUILD),
                        allow: 0,
                        deny: VIEW_CHANNEL,
                    }],
                ),
                channel(VOICE, "voice", ChannelKind::Other, Vec::new()),
            ],
        );
        // Newest first, like Discord.
        state.history.insert(
            GENERAL,
            vec![
                message(3, BOT_ID, "thinwire-bot", "reply from the bot"),
                message(2, 42, "alice", ""),
                message(1, 42, "alice", "hello guild"),
            ],
        );
        Self {
            state: Mutex::new(state),
            hold_history: None,
            hold_send: None,
            sends_at_hold: AtomicUsize::new(0),
            send_result_pause: None,
        }
    }

    pub(crate) fn state(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.state.lock().expect("fake state")
    }

    fn check_token(&self) -> Result<(), DiscordApiError> {
        let mut state = self.state();
        if state.unauthorized {
            return Err(DiscordApiError::Unauthorized);
        }
        if let Some(error) = state.next_error.take() {
            return Err(error);
        }
        Ok(())
    }
}

impl DiscordApi for FakeDiscordApi {
    fn bot_user_id(&self) -> ApiFuture<'_, u64> {
        Box::pin(async move {
            self.check_token()?;
            let hold = self.state().hold_load.clone();
            if let Some(hold) = hold {
                hold.notified().await;
            }
            Ok(BOT_ID)
        })
    }

    fn guilds(&self) -> ApiFuture<'_, Vec<GuildSummary>> {
        Box::pin(async move {
            self.check_token()?;
            Ok(self.state().guilds.clone())
        })
    }

    fn member_roles(&self, guild_id: u64, user_id: u64) -> ApiFuture<'_, Vec<u64>> {
        Box::pin(async move {
            self.check_token()?;
            assert_eq!(user_id, BOT_ID, "only the bot member is read");
            if guild_id == LOCKED_GUILD {
                return Err(DiscordApiError::Forbidden);
            }
            Ok(self
                .state()
                .roles
                .get(&guild_id)
                .cloned()
                .unwrap_or_default())
        })
    }

    fn channels(&self, guild_id: u64) -> ApiFuture<'_, Vec<ChannelSummary>> {
        Box::pin(async move {
            self.check_token()?;
            let channels = self
                .state()
                .channels
                .get(&guild_id)
                .cloned()
                .ok_or(DiscordApiError::NotFound)?;
            let (hold, arrived) = {
                let state = self.state();
                (
                    state.hold_channels.clone(),
                    state.channels_at_barrier.clone(),
                )
            };
            if let Some(arrived) = arrived {
                arrived.notify_one();
            }
            if let Some(hold) = hold {
                hold.notified().await;
            }
            Ok(channels)
        })
    }

    fn history(&self, channel_id: u64, limit: u16) -> ApiFuture<'_, Vec<MessageSummary>> {
        Box::pin(async move {
            self.check_token()?;
            if let Some(hold) = &self.hold_history {
                hold.notified().await;
            }
            let mut messages = self
                .state()
                .history
                .get(&channel_id)
                .cloned()
                .unwrap_or_default();
            messages.truncate(usize::from(limit));
            Ok(messages)
        })
    }

    fn send_result_pause(&self) -> Option<Arc<SendResultPause>> {
        self.send_result_pause.clone()
    }

    fn send(&self, channel_id: u64, body: String) -> ApiFuture<'_, MessageSummary> {
        Box::pin(async move {
            self.check_token()?;
            if let Some(hold) = &self.hold_send {
                self.sends_at_hold.fetch_add(1, Ordering::SeqCst);
                hold.notified().await;
                self.sends_at_hold.fetch_sub(1, Ordering::SeqCst);
            }
            let mut state = self.state();
            if let Some(error) = state.send_error {
                return Err(error);
            }
            state.next_id += 1;
            let id = state.next_id;
            state.sent.push((channel_id, body.clone()));
            Ok(message(id, BOT_ID, "thinwire-bot", &body))
        })
    }
}
