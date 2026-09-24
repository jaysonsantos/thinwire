//! Slack workspace-app inbox actor.
//!
//! One tokio task owns the session. `SlackInbox::handle` only queues a job,
//! so the adapter host never waits on Slack. The OAuth install and the
//! Socket Mode stream run as child tasks and report back through the queue.
//! Tokens stay in the vault and in this task. Events carry no secrets.

use std::collections::HashMap;
use std::fmt;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::AbortHandle;

use super::api::{
    SlackApiError, SlackAppToken, SlackBotToken, SlackBrowser, SlackChannel, SlackChannelKind,
    SlackCodeExchange, SlackEventSource, SlackEventStream, SlackInbound, SlackInstallGrant,
    SlackPost, SlackSocketScope, SlackWebApi,
};
use super::credentials::{SlackApiSource, resolve_slack_app_token, resolve_slack_client};
use super::install::{
    SLACK_OAUTH_LOOPBACK_PORT, SlackCallbackError, authorize_url, new_oauth_state,
};
use super::loopback::SlackLoopback;
use super::secrets::{SlackSecretKey, SlackSecretVault};
use super::{CAPABILITIES, SLACK_CONVERSATION_PREFIX};
use crate::adapter::{
    AdapterCommand, AdapterError, AdapterStatus, ChatMessage, Conversation, Delivery, EventTx,
    ProtocolAdapter, ProtocolCapabilities, ProtocolId, emit_conversation,
    emit_conversation_removed, emit_message, emit_message_body, emit_messages_removed,
    emit_send_accepted, emit_send_rejected, emit_status, emit_stopped,
};

/// Messages loaded when a channel opens.
const HISTORY_LIMIT: u16 = 50;

/// Pages of `conversations.list` fetched in one load. Each page is one Web API
/// call, so the live client's rate control applies between them. A later
/// `LoadChats` continues from the saved cursor when this cap stops the walk.
pub(super) const MAX_CHANNEL_PAGES: u32 = 50;

/// Time the workspace admin has to approve the install in the browser.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Slack error codes that mean the stored bot token no longer works.
const TOKEN_ENDED: &[&str] = &[
    "invalid_auth",
    "token_revoked",
    "token_expired",
    "account_inactive",
    "not_authed",
];

const DETAIL_NOT_CONNECTED: &str =
    "Slack workspace is not connected. Connect to install the thinwire workspace app.";
const DETAIL_WAITING: &str =
    "Approve the Slack workspace install in your browser. thinwire waits for Slack.";
const DETAIL_NO_BROWSER: &str = "Could not open a browser for the Slack install.";
const DETAIL_PORT_BUSY: &str = "Could not listen on 127.0.0.1:8976 for the Slack install. Close the app that uses this port and connect again.";
const DETAIL_TIMED_OUT: &str = "The Slack install timed out. Connect again to retry.";
const DETAIL_DECLINED: &str = "The Slack install was cancelled in the browser.";
const DETAIL_ENDED: &str =
    "Slack workspace access ended (token revoked or app removed). Connect to install again.";
const DETAIL_DISCONNECTED: &str =
    "Slack disconnected. The workspace install stays saved. Connect to resume.";
const DETAIL_NOT_IN_CHANNEL: &str =
    "The thinwire app is not in this channel. Add the app to the channel in Slack.";
const DETAIL_NOT_READY: &str = "Slack workspace is not connected yet.";
const DETAIL_NO_LIVE_EVENTS: &str = "Live updates are off: no Socket Mode app-level token.";

/// Worker-side dependencies. Tests pass fakes.
pub struct SlackDeps<A, S, B> {
    pub api: Arc<A>,
    pub socket: Arc<S>,
    pub browser: Arc<B>,
    pub vault: Arc<dyn SlackSecretVault>,
    pub source: SlackApiSource,
    /// Loopback address for the OAuth redirect. Live: `127.0.0.1:8976`.
    pub callback_addr: SocketAddr,
    pub install_timeout: Duration,
}

impl<A, S, B> SlackDeps<A, S, B> {
    /// Production address and time limit.
    #[must_use]
    pub fn new(
        api: A,
        socket: S,
        browser: B,
        vault: Arc<dyn SlackSecretVault>,
        source: SlackApiSource,
    ) -> Self {
        Self {
            api: Arc::new(api),
            socket: Arc::new(socket),
            browser: Arc::new(browser),
            vault,
            source,
            callback_addr: SocketAddr::from((Ipv4Addr::LOCALHOST, SLACK_OAUTH_LOOPBACK_PORT)),
            install_timeout: INSTALL_TIMEOUT,
        }
    }
}

enum Job {
    Command(AdapterCommand),
    Inbound(SlackInbound),
    Installed {
        attempt: u64,
        result: Result<SlackInstallGrant, InstallFailure>,
    },
}

#[derive(Debug)]
enum InstallFailure {
    TimedOut,
    Callback(SlackCallbackError),
    Exchange(SlackApiError),
}

/// Workspace-app inbox adapter. Generic over the Slack seams for tests.
pub struct SlackInbox<A, S, B>
where
    A: SlackWebApi,
    S: SlackEventSource,
    B: SlackBrowser,
{
    deps: Option<SlackDeps<A, S, B>>,
    jobs: Option<UnboundedSender<Job>>,
}

impl<A, S, B> SlackInbox<A, S, B>
where
    A: SlackWebApi,
    S: SlackEventSource,
    B: SlackBrowser,
{
    #[must_use]
    pub fn new(deps: SlackDeps<A, S, B>) -> Self {
        Self {
            deps: Some(deps),
            jobs: None,
        }
    }
}

impl<A, S, B> fmt::Debug for SlackInbox<A, S, B>
where
    A: SlackWebApi,
    S: SlackEventSource,
    B: SlackBrowser,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SlackInbox")
            .field("started", &self.jobs.is_some())
            .field("tokens", &"<redacted>")
            .finish()
    }
}

impl<A, S, B> ProtocolAdapter for SlackInbox<A, S, B>
where
    A: SlackWebApi,
    S: SlackEventSource,
    B: SlackBrowser,
{
    fn id(&self) -> ProtocolId {
        ProtocolId::Slack
    }

    fn capabilities(&self) -> ProtocolCapabilities {
        CAPABILITIES
    }

    fn start(&mut self, events: EventTx) {
        let Some(deps) = self.deps.take() else {
            return;
        };
        let (jobs_tx, jobs_rx) = unbounded_channel();
        let resume = deps.vault.get_secret(SlackSecretKey::BotToken).is_some();
        if !resume {
            emit_status(
                &events,
                ProtocolId::Slack,
                AdapterStatus::Stubbed,
                DETAIL_NOT_CONNECTED,
            );
        }
        let session = Session::new(deps, events, jobs_tx.clone());
        tokio::spawn(session.run(jobs_rx));
        if resume {
            let _ = jobs_tx.send(Job::Command(AdapterCommand::Connect {
                protocol: ProtocolId::Slack,
            }));
        }
        self.jobs = Some(jobs_tx);
    }

    fn handle(&mut self, command: AdapterCommand, _events: &EventTx) -> Result<(), AdapterError> {
        let Some(jobs) = &self.jobs else {
            return Err(AdapterError::Unavailable {
                protocol: ProtocolId::Slack,
                reason: "slack worker is not started",
            });
        };
        jobs.send(Job::Command(command))
            .map_err(|_| AdapterError::Unavailable {
                protocol: ProtocolId::Slack,
                reason: "slack worker stopped",
            })
    }

    fn shutdown(&mut self, events: &EventTx) {
        let Some(jobs) = &self.jobs else {
            emit_stopped(events, ProtocolId::Slack);
            return;
        };
        if jobs
            .send(Job::Command(AdapterCommand::Shutdown {
                protocol: ProtocolId::Slack,
            }))
            .is_err()
        {
            emit_stopped(events, ProtocolId::Slack);
        }
    }
}

struct Live<T> {
    token: SlackBotToken,
    team_id: String,
    app_id: String,
    team_name: String,
    bot_user_id: String,
    stream: Option<T>,
    forwarder: Option<AbortHandle>,
    next_cursor: Option<String>,
    list_done: bool,
}

struct Session<A, S, B>
where
    S: SlackEventSource,
{
    deps: SlackDeps<A, S, B>,
    events: EventTx,
    jobs: UnboundedSender<Job>,
    live: Option<Live<S::Stream>>,
    install: Option<(u64, AbortHandle)>,
    attempts: u64,
    channels: HashMap<String, Conversation>,
    names: HashMap<String, String>,
}

impl<A, S, B> Session<A, S, B>
where
    A: SlackWebApi,
    S: SlackEventSource,
    B: SlackBrowser,
{
    fn new(deps: SlackDeps<A, S, B>, events: EventTx, jobs: UnboundedSender<Job>) -> Self {
        Self {
            deps,
            events,
            jobs,
            live: None,
            install: None,
            attempts: 0,
            channels: HashMap::new(),
            names: HashMap::new(),
        }
    }

    async fn run(mut self, mut jobs: UnboundedReceiver<Job>) {
        while let Some(job) = jobs.recv().await {
            match job {
                Job::Command(command) => self.command(command).await,
                Job::Inbound(SlackInbound::Message(post)) => self.inbound(post).await,
                Job::Inbound(SlackInbound::Edited { channel, ts, text }) => {
                    self.edit_message(&channel, &ts, &text);
                }
                Job::Inbound(SlackInbound::Deleted { channel, ts }) => {
                    self.delete_message(&channel, &ts);
                }
                Job::Inbound(SlackInbound::Revoked) => self.revoked().await,
                Job::Installed { attempt, result } => self.installed(attempt, result).await,
            }
        }
        self.stop_live().await;
        self.cancel_install();
    }

    fn status(&self, status: AdapterStatus, detail: impl Into<String>) {
        emit_status(&self.events, ProtocolId::Slack, status, detail);
    }

    async fn command(&mut self, command: AdapterCommand) {
        match command {
            AdapterCommand::Connect {
                protocol: ProtocolId::Slack,
            } => self.connect().await,
            AdapterCommand::Disconnect {
                protocol: ProtocolId::Slack,
            } => {
                self.cancel_install();
                self.stop_live().await;
                self.status(AdapterStatus::Stubbed, DETAIL_DISCONNECTED);
            }
            AdapterCommand::LoadChats {
                protocol: ProtocolId::Slack,
            } => self.load_channels().await,
            AdapterCommand::OpenChat {
                protocol: ProtocolId::Slack,
                conversation_id,
            } => self.open_channel(&conversation_id).await,
            AdapterCommand::SendText {
                protocol: ProtocolId::Slack,
                conversation_id,
                body,
                request,
            } => self.send(&conversation_id, &body, request).await,
            AdapterCommand::Shutdown {
                protocol: ProtocolId::Slack,
            } => {
                self.cancel_install();
                self.stop_live().await;
                emit_stopped(&self.events, ProtocolId::Slack);
            }
            _ => self.status(
                AdapterStatus::Error,
                "command is not handled by the Slack workspace app",
            ),
        }
    }

    async fn connect(&mut self) {
        if let Some(live) = &self.live {
            let detail = ready_detail(&live.team_name, live.stream.is_some());
            self.status(AdapterStatus::Ready, detail);
            return;
        }
        if self.install.is_some() {
            self.status(AdapterStatus::Connecting, DETAIL_WAITING);
            return;
        }
        match self.deps.vault.get_secret(SlackSecretKey::BotToken) {
            Some(token) => self.go_live(SlackBotToken::new(token)).await,
            None => self.begin_install().await,
        }
    }

    async fn begin_install(&mut self) {
        let Some((client_id, client_secret, _)) =
            resolve_slack_client(self.deps.vault.as_ref(), &self.deps.source)
        else {
            self.status(
                AdapterStatus::Error,
                "Slack app credentials are missing in this build.",
            );
            return;
        };
        let Ok(listener) = SlackLoopback::bind(self.deps.callback_addr).await else {
            self.status(AdapterStatus::Error, DETAIL_PORT_BUSY);
            return;
        };
        let state = new_oauth_state();
        self.deps
            .vault
            .set_secret(SlackSecretKey::OAuthState, &state);
        if !self.deps.browser.open(&authorize_url(&client_id, &state)) {
            self.deps.vault.set_secret(SlackSecretKey::OAuthState, "");
            self.status(AdapterStatus::Error, DETAIL_NO_BROWSER);
            return;
        }
        self.attempts += 1;
        let attempt = self.attempts;
        let api = Arc::clone(&self.deps.api);
        let vault = Arc::clone(&self.deps.vault);
        let jobs = self.jobs.clone();
        let limit = self.deps.install_timeout;
        let task = tokio::spawn(async move {
            let result = match tokio::time::timeout(limit, listener.wait_for_code(&state)).await {
                Err(_) => Err(InstallFailure::TimedOut),
                Ok(Err(error)) => Err(InstallFailure::Callback(error)),
                Ok(Ok(code)) => {
                    vault.set_secret(SlackSecretKey::OAuthCode, &code);
                    api.exchange_code(SlackCodeExchange {
                        client_id,
                        client_secret,
                        code,
                    })
                    .await
                    .map_err(InstallFailure::Exchange)
                }
            };
            let _ = jobs.send(Job::Installed { attempt, result });
        });
        self.install = Some((attempt, task.abort_handle()));
        self.status(AdapterStatus::Connecting, DETAIL_WAITING);
    }

    fn cancel_install(&mut self) {
        if let Some((_, task)) = self.install.take() {
            task.abort();
        }
        self.deps.vault.set_secret(SlackSecretKey::OAuthState, "");
        self.deps.vault.set_secret(SlackSecretKey::OAuthCode, "");
    }

    async fn installed(&mut self, attempt: u64, result: Result<SlackInstallGrant, InstallFailure>) {
        if self.install.as_ref().map(|(current, _)| *current) != Some(attempt) {
            return;
        }
        self.install = None;
        self.deps.vault.set_secret(SlackSecretKey::OAuthState, "");
        self.deps.vault.set_secret(SlackSecretKey::OAuthCode, "");
        match result {
            Ok(grant) => {
                grant
                    .workspace
                    .remember(self.deps.vault.as_ref(), grant.bot_token.reveal());
                tracing::info!(
                    team_id = grant.workspace.team_id(),
                    "slack workspace app installed"
                );
                self.go_live(grant.bot_token).await;
            }
            Err(InstallFailure::TimedOut) => self.status(AdapterStatus::Error, DETAIL_TIMED_OUT),
            Err(InstallFailure::Callback(SlackCallbackError::Declined)) => {
                self.status(AdapterStatus::Error, DETAIL_DECLINED);
            }
            Err(InstallFailure::Callback(error)) => {
                self.status(AdapterStatus::Error, error.to_string());
            }
            Err(InstallFailure::Exchange(error)) => {
                self.status(
                    AdapterStatus::Error,
                    format!("Slack install failed: {error}."),
                );
            }
        }
    }

    async fn go_live(&mut self, token: SlackBotToken) {
        self.status(
            AdapterStatus::Connecting,
            "Connecting to the Slack workspace.",
        );
        let identity = match self.deps.api.identify(&token).await {
            Ok(identity) => identity,
            Err(error) => {
                self.api_failed(&error).await;
                return;
            }
        };
        let stored_app = self
            .deps
            .vault
            .get_secret(SlackSecretKey::AppId)
            .unwrap_or_default();
        let app_id = if identity.app_id().is_empty() {
            stored_app
        } else {
            identity.app_id().to_string()
        };
        self.live = Some(Live {
            token,
            team_id: identity.team_id().to_string(),
            app_id,
            team_name: identity.team_name().to_string(),
            bot_user_id: identity.bot_user_id().to_string(),
            stream: None,
            forwarder: None,
            next_cursor: None,
            list_done: false,
        });
        self.start_events().await;
        let live_events = self.live.as_ref().is_some_and(|live| live.stream.is_some());
        self.status(
            AdapterStatus::Ready,
            ready_detail(identity.team_name(), live_events),
        );
        self.load_channels().await;
    }

    async fn start_events(&mut self) {
        let Some((app_token, _)) =
            resolve_slack_app_token(self.deps.vault.as_ref(), &self.deps.source)
        else {
            return;
        };
        let (sink, mut inbound) = unbounded_channel();
        let scope = self
            .live
            .as_ref()
            .map(|live| SlackSocketScope {
                team_id: live.team_id.clone(),
                app_id: live.app_id.clone(),
            })
            .unwrap_or(SlackSocketScope {
                team_id: String::new(),
                app_id: String::new(),
            });
        let stream = match self
            .deps
            .socket
            .connect(SlackAppToken::new(app_token), scope, sink)
            .await
        {
            Ok(stream) => stream,
            Err(error) => {
                tracing::warn!(%error, "slack socket mode did not start");
                return;
            }
        };
        let jobs = self.jobs.clone();
        let forwarder = tokio::spawn(async move {
            while let Some(event) = inbound.recv().await {
                if jobs.send(Job::Inbound(event)).is_err() {
                    break;
                }
            }
        });
        if let Some(live) = &mut self.live {
            live.stream = Some(stream);
            live.forwarder = Some(forwarder.abort_handle());
        }
    }

    async fn stop_live(&mut self) {
        let Some(mut live) = self.live.take() else {
            return;
        };
        if let Some(forwarder) = live.forwarder.take() {
            forwarder.abort();
        }
        if let Some(stream) = live.stream.take() {
            stream.stop().await;
        }
        self.retract_channels();
    }

    /// The shell drops a row only after `ConversationRemoved`. Emit one for
    /// every listed channel before the map is discarded.
    fn retract_channels(&mut self) {
        let ids: Vec<String> = self.channels.values().map(|row| row.id.clone()).collect();
        for id in ids {
            emit_conversation_removed(&self.events, ProtocolId::Slack, id);
        }
        self.channels.clear();
    }

    async fn revoked(&mut self) {
        if self.live.is_none() {
            return;
        }
        self.end_access().await;
    }

    /// The bot token no longer works. Stop Socket Mode and drop the install.
    async fn end_access(&mut self) {
        self.stop_live().await;
        self.deps.vault.set_secret(SlackSecretKey::BotToken, "");
        self.deps.vault.set_secret(SlackSecretKey::TeamId, "");
        self.deps.vault.set_secret(SlackSecretKey::AppId, "");
        self.status(AdapterStatus::Error, DETAIL_ENDED);
    }

    async fn api_failed(&mut self, error: &SlackApiError) {
        if let SlackApiError::Api(code) = error
            && TOKEN_ENDED.contains(&code.as_str())
        {
            self.end_access().await;
            return;
        }
        if matches!(error, SlackApiError::Api(code) if code == "not_in_channel") {
            self.status(AdapterStatus::Error, DETAIL_NOT_IN_CHANNEL);
            return;
        }
        self.status(AdapterStatus::Error, format!("{error}."));
    }

    async fn load_channels(&mut self) {
        let Some(live) = &self.live else {
            self.status(AdapterStatus::Error, DETAIL_NOT_READY);
            return;
        };
        if live.list_done {
            return;
        }
        let token = live.token.clone();
        for _ in 0..MAX_CHANNEL_PAGES {
            if self.live.as_ref().is_none_or(|live| live.list_done) {
                return;
            }
            let cursor = self.live.as_ref().and_then(|live| live.next_cursor.clone());
            let page = match self.deps.api.list_channels(&token, cursor).await {
                Ok(page) => page,
                Err(error) => {
                    self.api_failed(&error).await;
                    return;
                }
            };
            let next = page.next_cursor.filter(|cursor| !cursor.is_empty());
            if let Some(live) = &mut self.live {
                live.next_cursor.clone_from(&next);
                live.list_done = next.is_none();
            }
            for channel in page.channels {
                if !channel.is_member {
                    continue;
                }
                let title = self.channel_title(&token, &channel).await;
                let conversation = Conversation {
                    protocol: ProtocolId::Slack,
                    id: conversation_id(&channel.id),
                    title,
                    participant: participant(channel.kind).into(),
                    preview: String::new(),
                    unread: 0,
                    order: 0,
                    last_at: 0,
                    is_group: is_group(channel.kind),
                };
                if let Some(existing) = self.channels.get(&channel.id) {
                    // Keep live preview, unread, order, and time from Socket Mode.
                    let mut merged = conversation;
                    merged.preview.clone_from(&existing.preview);
                    merged.unread = existing.unread;
                    merged.order = existing.order;
                    merged.last_at = existing.last_at;
                    self.upsert(channel.id, merged);
                } else {
                    self.upsert(channel.id, conversation);
                }
            }
        }
    }

    fn upsert(&mut self, channel: String, conversation: Conversation) {
        emit_conversation(&self.events, conversation.clone());
        self.channels.insert(channel, conversation);
    }

    async fn channel_title(&mut self, token: &SlackBotToken, channel: &SlackChannel) -> String {
        match channel.kind {
            SlackChannelKind::Public | SlackChannelKind::Private => format!("#{}", channel.name),
            SlackChannelKind::GroupMessage => group_title(&channel.name),
            SlackChannelKind::DirectMessage => match &channel.dm_user {
                Some(user) => self.user_name(token, user).await,
                None => "Direct message".into(),
            },
        }
    }

    async fn user_name(&mut self, token: &SlackBotToken, user: &str) -> String {
        if let Some(name) = self.names.get(user) {
            return name.clone();
        }
        let Ok(name) = self.deps.api.user_name(token, user).await else {
            return user.to_string();
        };
        if name.trim().is_empty() {
            return user.to_string();
        }
        self.names.insert(user.to_string(), name.clone());
        name
    }

    async fn open_channel(&mut self, conversation: &str) {
        let Some(channel) = channel_id(conversation) else {
            self.status(AdapterStatus::Error, "Unknown Slack conversation.");
            return;
        };
        let Some(live) = &self.live else {
            self.status(AdapterStatus::Error, DETAIL_NOT_READY);
            return;
        };
        let token = live.token.clone();
        let posts = match self.deps.api.history(&token, channel, HISTORY_LIMIT).await {
            Ok(posts) => posts,
            Err(error) => {
                self.api_failed(&error).await;
                return;
            }
        };
        for post in posts.into_iter().rev() {
            let message = self.chat_message(&token, post).await;
            emit_message(&self.events, message);
        }
        if let Some(row) = self.channels.get_mut(channel)
            && row.unread != 0
        {
            row.unread = 0;
            emit_conversation(&self.events, row.clone());
        }
    }

    async fn send(&mut self, conversation: &str, body: &str, request: u64) {
        let Some(channel) = channel_id(conversation) else {
            emit_send_rejected(&self.events, ProtocolId::Slack, conversation, request);
            self.status(AdapterStatus::Error, "Unknown Slack conversation.");
            return;
        };
        let Some(live) = &self.live else {
            emit_send_rejected(&self.events, ProtocolId::Slack, conversation, request);
            self.status(AdapterStatus::Error, DETAIL_NOT_READY);
            return;
        };
        let token = live.token.clone();
        match self.deps.api.post_message(&token, channel, body).await {
            Ok(post) => {
                let message = self.chat_message(&token, post).await;
                emit_send_accepted(
                    &self.events,
                    ProtocolId::Slack,
                    message.conversation_id.clone(),
                    request,
                );
                emit_message(&self.events, message);
            }
            Err(error) => {
                emit_send_rejected(&self.events, ProtocolId::Slack, conversation, request);
                self.api_failed(&error).await;
            }
        }
    }

    fn edit_message(&mut self, channel: &str, ts: &str, text: &str) {
        emit_message_body(
            &self.events,
            ProtocolId::Slack,
            conversation_id(channel),
            message_id(channel, ts),
            text,
        );
        let Some(row) = self.channels.get(channel) else {
            return;
        };
        if ts_order(ts) != row.last_at || row.last_at == 0 {
            return;
        }
        let mut row = row.clone();
        row.preview = text.to_string();
        self.upsert(channel.to_string(), row);
    }

    fn delete_message(&self, channel: &str, ts: &str) {
        emit_messages_removed(
            &self.events,
            ProtocolId::Slack,
            conversation_id(channel),
            vec![message_id(channel, ts)],
        );
    }

    async fn inbound(&mut self, post: SlackPost) {
        let Some(live) = &self.live else {
            return;
        };
        let token = live.token.clone();
        let channel = post.channel.clone();
        let order = ts_order(&post.ts);
        let message = self.chat_message(&token, post).await;
        let preview = message.body.clone();
        let outbound = message.outbound;
        let sender = message.sender.clone();
        emit_message(&self.events, message);
        let Some(row) = self.channels.get(&channel) else {
            // A new DM or a channel the bot just joined. `conversations.list` fills the
            // real title on the next load; until then a DM shows the sender.
            let title = if channel.starts_with('D') {
                sender
            } else {
                format!("#{channel}")
            };
            let conversation = Conversation {
                protocol: ProtocolId::Slack,
                id: conversation_id(&channel),
                title,
                participant: "workspace".into(),
                preview,
                unread: u32::from(!outbound),
                order,
                last_at: order,
                is_group: !channel.starts_with('D'),
            };
            // The title is only the id. A later LoadChats walks the list again
            // and replaces it with the channel name.
            if let Some(live) = &mut self.live {
                live.list_done = false;
                live.next_cursor = None;
            }
            self.upsert(channel, conversation);
            return;
        };
        let mut row = row.clone();
        row.preview = preview;
        row.order = row.order.max(order);
        row.last_at = row.last_at.max(order);
        if !outbound {
            row.unread = row.unread.saturating_add(1);
        }
        self.upsert(channel, row);
    }

    async fn chat_message(&mut self, token: &SlackBotToken, post: SlackPost) -> ChatMessage {
        let outbound = self
            .live
            .as_ref()
            .is_some_and(|live| post.user.as_deref() == Some(live.bot_user_id.as_str()));
        let sender = match (&post.user, &post.username) {
            (_, Some(username)) if !username.trim().is_empty() => username.clone(),
            (Some(user), _) => self.user_name(token, user).await,
            (None, _) => "Slack".into(),
        };
        ChatMessage {
            protocol: ProtocolId::Slack,
            conversation_id: conversation_id(&post.channel),
            id: message_id(&post.channel, &post.ts),
            sender,
            body: post.text,
            outbound,
            delivery: Delivery::Sent,
            sent_at: ts_order(&post.ts),
        }
    }
}

fn ready_detail(team_name: &str, live_events: bool) -> String {
    let mut detail = format!(
        "Slack workspace {team_name}. Workspace app: it reads only the channels the app was added to."
    );
    if !live_events {
        detail.push(' ');
        detail.push_str(DETAIL_NO_LIVE_EVENTS);
    }
    detail
}

fn conversation_id(channel: &str) -> String {
    format!("{SLACK_CONVERSATION_PREFIX}{channel}")
}

/// `slack:<channel>:<rank>`. The shell parses the last segment as `i64`.
fn message_id(channel: &str, ts: &str) -> String {
    format!("{SLACK_CONVERSATION_PREFIX}{channel}:{}", ts_rank(ts))
}

fn channel_id(conversation: &str) -> Option<&str> {
    conversation
        .strip_prefix(SLACK_CONVERSATION_PREFIX)
        .filter(|id| !id.is_empty() && !id.contains(':'))
}

const fn is_group(kind: SlackChannelKind) -> bool {
    !matches!(kind, SlackChannelKind::DirectMessage)
}

const fn participant(kind: SlackChannelKind) -> &'static str {
    match kind {
        SlackChannelKind::Public => "channel",
        SlackChannelKind::Private => "private channel",
        SlackChannelKind::DirectMessage => "direct message",
        SlackChannelKind::GroupMessage => "group message",
    }
}

/// `mpdm-ana--bo--cy-1` → `ana, bo, cy`.
fn group_title(name: &str) -> String {
    let trimmed = name.strip_prefix("mpdm-").unwrap_or(name);
    let trimmed = trimmed
        .rsplit_once('-')
        .filter(|(_, tail)| tail.chars().all(|ch| ch.is_ascii_digit()))
        .map_or(trimmed, |(head, _)| head);
    trimmed.split("--").collect::<Vec<_>>().join(", ")
}

/// Slack `ts` seconds as a sort key. Newer sorts first.
fn ts_order(ts: &str) -> i64 {
    ts.split('.')
        .next()
        .and_then(|seconds| seconds.parse().ok())
        .unwrap_or(0)
}

/// Slack `ts` (`seconds.microseconds`) as the integer the shell sorts on.
/// A dotted timestamp does not parse as `i64`, so every message would rank 0.
fn ts_rank(ts: &str) -> i64 {
    let (seconds, fraction) = ts.split_once('.').unwrap_or((ts, ""));
    let Ok(seconds) = seconds.parse::<i64>() else {
        return 0;
    };
    if seconds < 0 {
        return 0;
    }
    let digits: String = fraction
        .chars()
        .filter(|ch| ch.is_ascii_digit())
        .take(6)
        .collect();
    let micros = if digits.is_empty() {
        0
    } else {
        format!("{digits:0<6}").parse::<i64>().unwrap_or(0)
    };
    seconds.saturating_mul(1_000_000).saturating_add(micros)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_and_reject_foreign_ids() {
        assert_eq!(conversation_id("C123"), "slack:C123");
        assert_eq!(channel_id("slack:C123"), Some("C123"));
        assert_eq!(channel_id("telegram:1"), None);
        assert_eq!(channel_id("slack:"), None);
        assert_eq!(channel_id("slack:C1:2"), None);
    }

    #[test]
    fn group_titles_read_as_names() {
        assert_eq!(group_title("mpdm-ana--bo--cy-1"), "ana, bo, cy");
        assert_eq!(group_title("mpdm-ana--bo"), "ana, bo");
        assert_eq!(ts_order("1712345678.000200"), 1_712_345_678);
        assert_eq!(ts_order("bad"), 0);
        assert_eq!(ts_rank("1700000001.000100"), 1_700_000_001_000_100);
        assert_eq!(ts_rank("1700000200.000100"), 1_700_000_200_000_100);
        assert!(ts_rank("1700000001.000100") < ts_rank("1700000200.000100"));
        assert_eq!(
            message_id("C1", "1700000001.000100"),
            "slack:C1:1700000001000100"
        );
        assert_eq!(ts_rank("bad"), 0);
    }
}
