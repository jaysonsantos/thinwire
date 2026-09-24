//! Live Slack seams on `slack-morphism` (Web API over hyper, Socket Mode over
//! tungstenite). Compiled only with `--features slack-oauth`.
//!
//! Runs on the tokio worker. Error mapping keeps only Slack's short error
//! code, never a response body. The `slack_morphism` log target must stay
//! off in the binary, because the library logs the one-time Socket Mode URL.

use std::sync::{Arc, Mutex};

use slack_morphism::errors::SlackClientError;
use slack_morphism::prelude::*;
use tokio::sync::mpsc::UnboundedSender;

use super::api::{
    SlackApiError, SlackAppToken, SlackBotToken, SlackBrowser, SlackChannel, SlackChannelKind,
    SlackChannelPage, SlackCodeExchange, SlackEventSource, SlackEventStream, SlackInbound,
    SlackInstallGrant, SlackPost, SlackWebApi,
};
use super::install::SlackInstalledWorkspace;
use super::morphism::{oauth_v2_access_request, workspace_bot_token};

/// Retries after Slack sends `Retry-After`. The library retries only when this is set.
const RATE_LIMIT_RETRIES: usize = 3;

/// Page size for `conversations.list`. Slack allows up to 1000.
const CHANNEL_PAGE: u16 = 200;

/// Placeholder body for a message with no text (files, blocks only).
const NO_TEXT: &str = "(no text)";

fn map_error(error: SlackClientError) -> SlackApiError {
    match error {
        SlackClientError::ApiError(api) => SlackApiError::api(&api.code),
        SlackClientError::RateLimitError(_) => SlackApiError::RateLimited,
        _ => SlackApiError::Network,
    }
}

fn api_token(value: &str) -> SlackApiToken {
    SlackApiToken::new(value.into())
}

/// Bot token for Web API calls. A known team id turns on per-team method tiers.
fn session_token(value: &str, team_id: Option<&str>) -> SlackApiToken {
    match team_id.map(str::trim).filter(|id| !id.is_empty()) {
        Some(team) => workspace_bot_token(value, team),
        None => {
            let mut token = SlackApiToken::new(value.into());
            token.token_type = Some(SlackApiTokenType::Bot);
            token
        }
    }
}

fn rate_control_config() -> SlackApiRateControlConfig {
    SlackApiRateControlConfig::new().with_max_retries(RATE_LIMIT_RETRIES)
}

/// Shared hyper client. Build once on the worker.
pub fn hyper_client() -> std::io::Result<Arc<SlackHyperClient>> {
    // twilight (discord-bot) and hyper-rustls both use `ring`. Install it once.
    let _ = rustls::crypto::ring::default_provider().install_default();
    Ok(Arc::new(SlackClient::new(
        SlackClientHyperConnector::new()?.with_rate_control(rate_control_config()),
    )))
}

/// Web API on the official Slack endpoints with a workspace bot token.
pub struct MorphismWebApi {
    client: Arc<SlackHyperClient>,
    /// Bot user id from `auth.test` or the install. Used to find a DM peer.
    bot_user: Mutex<Option<String>>,
    /// Workspace id from `auth.test` or the install. Enables per-team rate tiers.
    team_id: Mutex<Option<String>>,
}

impl MorphismWebApi {
    #[must_use]
    pub fn new(client: Arc<SlackHyperClient>) -> Self {
        Self {
            client,
            bot_user: Mutex::new(None),
            team_id: Mutex::new(None),
        }
    }

    fn remember_team(&self, team: &str) {
        if team.is_empty() {
            return;
        }
        if let Ok(mut slot) = self.team_id.lock() {
            *slot = Some(team.to_string());
        }
    }

    fn team_id(&self) -> Option<String> {
        self.team_id.lock().ok().and_then(|slot| slot.clone())
    }

    fn bot_session_token(&self, value: &str) -> SlackApiToken {
        session_token(value, self.team_id().as_deref())
    }

    fn remember_bot_user(&self, user: &str) {
        if let Ok(mut slot) = self.bot_user.lock() {
            *slot = Some(user.to_string());
        }
    }

    fn bot_user(&self) -> Option<String> {
        self.bot_user.lock().ok().and_then(|slot| slot.clone())
    }

    async fn dm_peer(&self, token: &SlackApiToken, channel: &SlackChannelId) -> Option<String> {
        let bot = self.bot_user();
        let request = SlackApiConversationsMembersRequest::new()
            .with_channel(channel.clone())
            .with_limit(10);
        let members = self
            .client
            .open_session(token)
            .conversations_members(&request)
            .await
            .ok()?
            .members;
        members
            .into_iter()
            .map(|member| member.to_string())
            .find(|member| Some(member) != bot.as_ref())
    }
}

impl SlackWebApi for MorphismWebApi {
    async fn identify(
        &self,
        token: &SlackBotToken,
    ) -> Result<SlackInstalledWorkspace, SlackApiError> {
        let token = self.bot_session_token(token.reveal());
        let response = self
            .client
            .open_session(&token)
            .auth_test()
            .await
            .map_err(map_error)?;
        if response.bot_id.is_none() {
            return Err(SlackApiError::api("not_a_bot_token"));
        }
        self.remember_bot_user(response.user_id.value());
        self.remember_team(response.team_id.value());
        Ok(SlackInstalledWorkspace::new(
            response.team_id.value(),
            &response.team,
            response.user_id.value(),
            "",
        ))
    }

    async fn exchange_code(
        &self,
        exchange: SlackCodeExchange,
    ) -> Result<SlackInstallGrant, SlackApiError> {
        let request =
            oauth_v2_access_request(&exchange.client_id, &exchange.client_secret, &exchange.code);
        let response = self
            .client
            .oauth2_access(&request)
            .await
            .map_err(map_error)?;
        if response.token_type != SlackApiTokenType::Bot {
            return Err(SlackApiError::api("not_a_bot_token"));
        }
        let bot_user = response
            .bot_user_id
            .as_ref()
            .map(|id| id.value().clone())
            .unwrap_or_default();
        self.remember_bot_user(&bot_user);
        self.remember_team(response.team.id.value());
        let team_name = response.team.name.clone().unwrap_or_default();
        Ok(SlackInstallGrant {
            workspace: SlackInstalledWorkspace::new(
                response.team.id.value(),
                &team_name,
                &bot_user,
                response.app_id.value(),
            ),
            bot_token: SlackBotToken::new(response.access_token.value().clone()),
        })
    }

    async fn list_channels(
        &self,
        token: &SlackBotToken,
        cursor: Option<String>,
    ) -> Result<SlackChannelPage, SlackApiError> {
        let token = self.bot_session_token(token.reveal());
        let request = SlackApiConversationsListRequest::new()
            .with_types(vec![
                SlackConversationType::Public,
                SlackConversationType::Private,
                SlackConversationType::Im,
                SlackConversationType::Mpim,
            ])
            .with_exclude_archived(true)
            .with_limit(CHANNEL_PAGE)
            .opt_cursor(cursor.map(SlackCursorId::new));
        let response = self
            .client
            .open_session(&token)
            .conversations_list(&request)
            .await
            .map_err(map_error)?;
        let mut channels = Vec::with_capacity(response.channels.len());
        for info in response.channels {
            let flags = &info.flags;
            let kind = if flags.is_im == Some(true) {
                SlackChannelKind::DirectMessage
            } else if flags.is_mpim == Some(true) {
                SlackChannelKind::GroupMessage
            } else if flags.is_private == Some(true) || flags.is_group == Some(true) {
                SlackChannelKind::Private
            } else {
                SlackChannelKind::Public
            };
            let dm = kind == SlackChannelKind::DirectMessage;
            // IM and MPIM rows omit `is_member`; the bot is always in them.
            let is_member = flags
                .is_member
                .unwrap_or(dm || kind == SlackChannelKind::GroupMessage);
            let dm_user = if dm {
                self.dm_peer(&token, &info.id).await
            } else {
                None
            };
            channels.push(SlackChannel {
                id: info.id.to_string(),
                name: info.name.clone().unwrap_or_default(),
                kind,
                is_member,
                dm_user,
            });
        }
        let next_cursor = response
            .response_metadata
            .and_then(|meta| meta.next_cursor)
            .map(|cursor| cursor.to_string())
            .filter(|cursor| !cursor.is_empty());
        Ok(SlackChannelPage {
            channels,
            next_cursor,
        })
    }

    async fn history(
        &self,
        token: &SlackBotToken,
        channel: &str,
        limit: u16,
    ) -> Result<Vec<SlackPost>, SlackApiError> {
        let token = self.bot_session_token(token.reveal());
        let request = SlackApiConversationsHistoryRequest::new()
            .with_channel(SlackChannelId::new(channel.into()))
            .with_limit(limit);
        let response = self
            .client
            .open_session(&token)
            .conversations_history(&request)
            .await
            .map_err(map_error)?;
        Ok(response
            .messages
            .into_iter()
            .filter(|message| shown_subtype(message.subtype.as_ref()))
            .filter(|message| !is_thread_reply(&message.origin))
            .map(|message| SlackPost {
                channel: channel.to_string(),
                ts: message.origin.ts.to_string(),
                user: message.sender.user.map(|user| user.to_string()),
                username: message.sender.username,
                text: body(message.content.text),
            })
            .collect())
    }

    async fn post_message(
        &self,
        token: &SlackBotToken,
        channel: &str,
        text: &str,
    ) -> Result<SlackPost, SlackApiError> {
        let token = self.bot_session_token(token.reveal());
        let request = SlackApiChatPostMessageRequest::new(
            SlackChannelId::new(channel.into()),
            SlackMessageContent::new().with_text(text.into()),
        );
        let response = self
            .client
            .open_session(&token)
            .chat_post_message(&request)
            .await
            .map_err(map_error)?;
        Ok(SlackPost {
            channel: response.channel.to_string(),
            ts: response.ts.to_string(),
            user: response
                .message
                .sender
                .user
                .map(|user| user.to_string())
                .or_else(|| self.bot_user()),
            username: None,
            text: text.to_string(),
        })
    }

    async fn user_name(&self, token: &SlackBotToken, user: &str) -> Result<String, SlackApiError> {
        let token = self.bot_session_token(token.reveal());
        let request = SlackApiUsersInfoRequest::new(SlackUserId::new(user.into()));
        let response = self
            .client
            .open_session(&token)
            .users_info(&request)
            .await
            .map_err(map_error)?;
        let user = response.user;
        let profile = user.profile.as_ref();
        let pick = |value: Option<&String>| value.filter(|name| !name.trim().is_empty()).cloned();
        Ok(pick(profile.and_then(|p| p.display_name.as_ref()))
            .or_else(|| pick(profile.and_then(|p| p.real_name.as_ref())))
            .or_else(|| pick(user.real_name.as_ref()))
            .or_else(|| pick(user.name.as_ref()))
            .unwrap_or_else(|| user.id.to_string()))
    }
}

fn body(text: Option<String>) -> String {
    text.filter(|text| !text.trim().is_empty())
        .unwrap_or_else(|| NO_TEXT.into())
}

/// Plain messages and the subtypes a reader expects in a channel view.
fn shown_subtype(subtype: Option<&SlackMessageEventType>) -> bool {
    matches!(
        subtype,
        None | Some(
            SlackMessageEventType::BotMessage
                | SlackMessageEventType::MeMessage
                | SlackMessageEventType::FileShare
                | SlackMessageEventType::ThreadBroadcast
        )
    )
}

/// Thread replies are not in `conversations.history`, so live events drop them too.
fn is_thread_reply(origin: &SlackMessageOrigin) -> bool {
    origin
        .thread_ts
        .as_ref()
        .is_some_and(|thread| *thread != origin.ts)
}

/// Opens the install page in the system browser. Never logs the URL.
#[derive(Debug, Default)]
pub struct SystemBrowser;

impl SlackBrowser for SystemBrowser {
    fn open(&self, url: &str) -> bool {
        webbrowser::open(url).is_ok()
    }
}

/// Socket Mode on the app-level token.
pub struct MorphismSocket {
    client: Arc<SlackHyperClient>,
}

impl MorphismSocket {
    #[must_use]
    pub fn new(client: Arc<SlackHyperClient>) -> Self {
        Self { client }
    }
}

struct InboundSink(UnboundedSender<SlackInbound>);

/// Running Socket Mode listener. `stop` closes every connection.
pub struct MorphismStream {
    listener: SlackClientSocketModeListener<SlackClientHyperHttpsConnector>,
}

impl SlackEventStream for MorphismStream {
    async fn stop(self) {
        self.listener.shutdown().await;
    }
}

impl SlackEventSource for MorphismSocket {
    type Stream = MorphismStream;

    async fn connect(
        &self,
        app_token: SlackAppToken,
        sink: UnboundedSender<SlackInbound>,
    ) -> Result<MorphismStream, SlackApiError> {
        let environment = Arc::new(
            SlackClientEventsListenerEnvironment::new(Arc::clone(&self.client))
                .with_error_handler(on_error)
                .with_user_state(InboundSink(sink)),
        );
        let callbacks = SlackSocketModeListenerCallbacks::new().with_push_events(on_push);
        let listener = SlackClientSocketModeListener::new(
            &super::morphism::socket_mode_config(),
            environment,
            callbacks,
        );
        listener
            .listen_for(&api_token(app_token.reveal()))
            .await
            .map_err(map_error)?;
        listener.start().await;
        Ok(MorphismStream { listener })
    }
}

fn on_error(
    _error: Box<dyn std::error::Error + Send + Sync + 'static>,
    _client: Arc<SlackHyperClient>,
    _state: SlackClientEventsUserState,
) -> HttpStatusCode {
    // The error can carry a payload. Log only that it happened.
    tracing::warn!("slack socket mode listener error");
    HttpStatusCode::OK
}

async fn on_push(
    event: SlackPushEventCallback,
    _client: Arc<SlackHyperClient>,
    state: SlackClientEventsUserState,
) -> UserCallbackResult<()> {
    let inbound = match event.event {
        SlackEventCallbackBody::Message(message) => {
            if !shown_subtype(message.subtype.as_ref()) || is_thread_reply(&message.origin) {
                return Ok(());
            }
            let Some(channel) = message.origin.channel.as_ref() else {
                return Ok(());
            };
            SlackInbound::Message(SlackPost {
                channel: channel.to_string(),
                ts: message.origin.ts.to_string(),
                user: message.sender.user.map(|user| user.to_string()),
                username: message.sender.username,
                text: body(message.content.and_then(|content| content.text)),
            })
        }
        SlackEventCallbackBody::AppUninstalled(_) => SlackInbound::Revoked,
        _ => return Ok(()),
    };
    let guard = state.read().await;
    if let Some(InboundSink(sink)) = guard.get_user_state::<InboundSink>() {
        let _ = sink.send(inbound);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_control_retries_after_slack_says_wait() {
        let config = rate_control_config();
        assert_eq!(config.max_retries, Some(RATE_LIMIT_RETRIES));
        assert!(
            config
                .tiers_limits
                .contains_key(&SlackApiMethodRateTier::Tier4)
        );
    }

    #[test]
    fn session_token_carries_the_team_when_known() {
        let known = session_token("xoxb-test", Some("T1"));
        assert_eq!(known.token_type, Some(SlackApiTokenType::Bot));
        assert_eq!(
            known.team_id.as_ref().map(|id| id.value().as_str()),
            Some("T1")
        );
        let unknown = session_token("xoxb-test", None);
        assert!(unknown.team_id.is_none());
        let blank = session_token("xoxb-test", Some("  "));
        assert!(blank.team_id.is_none());
    }
}
