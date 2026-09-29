//! Slack workspace-app inbox actor.
//!
//! One tokio task owns the session. `SlackInbox::handle` only queues a job,
//! so the adapter host never waits on Slack. The OAuth install and the
//! Socket Mode stream run as child tasks and report back through the queue.
//! Tokens stay in the vault and in this task. Events carry no secrets.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::AbortHandle;

use super::api::{
    SlackApiError, SlackAppToken, SlackBotToken, SlackBrowser, SlackChannel, SlackChannelKind,
    SlackCodeExchange, SlackEventSource, SlackEventStream, SlackHistoryPage, SlackInbound,
    SlackInstallGrant, SlackPost, SlackSocketScope, SlackWebApi,
};
use super::credentials::{SlackApiSource, resolve_slack_app_token, resolve_slack_client};
use super::install::{
    SLACK_OAUTH_LOOPBACK_PORT, SlackCallbackError, authorize_url, new_oauth_state,
};
use super::loopback::SlackLoopback;
use super::secrets::{SlackSecretKey, SlackSecretVault};
use super::{CAPABILITIES, SLACK_CONVERSATION_PREFIX};
use crate::adapter::{
    AccountState, AdapterCommand, AdapterError, AdapterStatus, ChatMessage, Conversation, Delivery,
    EventTx, ProtocolAdapter, ProtocolCapabilities, ProtocolId, emit_account,
    emit_chat_list_loaded, emit_command_failed, emit_conversation, emit_conversation_removed,
    emit_flush_secrets, emit_history_loaded, emit_message, emit_message_body,
    emit_messages_removed, emit_notice, emit_send_accepted, emit_send_rejected, emit_status,
    emit_stopped,
};

/// Messages loaded when a channel opens. Also the cap on the per-channel
/// `shown` display cache.
pub(super) const HISTORY_LIMIT: u16 = 50;

/// How long a Slack post identity stays deduped after its timestamp.
/// Matches `thinwire_core::notify::STALE_AFTER_SECS`: a Socket Mode retry
/// can still notify while the post is that fresh, so liveness cannot follow
/// the `HISTORY_LIMIT` display cache. A post at exactly this age still
/// notifies (`now - sent_at > STALE_AFTER_SECS`), so it stays deduped too.
pub(super) const DEDUP_FRESH_SECS: i64 = 300;

/// Per-channel cap on deduped identities. Age eviction keeps every identity
/// inside `DEDUP_FRESH_SECS`. This drops the oldest only when one channel
/// exceeds it inside that window.
const DEDUP_LIMIT: usize = 4096;

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
    /// The chat the shell says the user is looking at. `None` leaves them all.
    View(Option<String>),
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
            emit_account(&events, ProtocolId::Slack, AccountState::Unlinked);
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

    fn view_chat(&mut self, conversation_id: Option<&str>, _events: &EventTx) {
        let Some(jobs) = &self.jobs else {
            return;
        };
        let _ = jobs.send(Job::View(conversation_id.map(str::to_string)));
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
    /// Member ids from the current `conversations.list` walk, including
    /// pages from an earlier `LoadChats` that stopped at the page cap.
    list_seen: HashSet<String>,
    /// Channels a send already refused. A later list must not turn the
    /// composer back on. Open and a successful post clear an id here.
    read_only: HashSet<String>,
    /// `restricted_action` is a workspace policy. It is not one channel.
    workspace_denied: bool,
    /// Channels to turn back on when `workspace_denied` clears. The set starts
    /// as the rows that were writable when the policy arrived, then gains a
    /// channel learned during the denial when Slack would allow a post.
    policy_writable: HashSet<String>,
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
    /// Channel the user is looking at. Inbound messages there stay read.
    viewing: Option<String>,
    /// Texts this session has shown, newest kept, so a delete can move the preview.
    /// `counted` is true when that post increased `unread`. Capped at
    /// `HISTORY_LIMIT`. A post still listed here is not live again. Posts that
    /// have left this cache stay deduped in `deduped`.
    shown: HashMap<String, Vec<Shown>>,
    /// Post `ts` and `client_msg_id` values kept after `shown` evicts them,
    /// for at least `DEDUP_FRESH_SECS`. A Socket Mode retry stays `History`
    /// while its identity is here.
    deduped: HashMap<String, Vec<Deduped>>,
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
            viewing: None,
            shown: HashMap::new(),
            deduped: HashMap::new(),
        }
    }

    async fn run(mut self, mut jobs: UnboundedReceiver<Job>) {
        while let Some(job) = jobs.recv().await {
            match job {
                Job::Command(command) => self.command(command).await,
                Job::View(conversation_id) => self.set_view(conversation_id),
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

    fn account(&self, state: AccountState) {
        emit_account(&self.events, ProtocolId::Slack, state);
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
                self.account(AccountState::Unlinked);
                self.status(AdapterStatus::Stubbed, DETAIL_DISCONNECTED);
            }
            AdapterCommand::LoadChats {
                protocol: ProtocolId::Slack,
            } => {
                if self.live.as_ref().is_some_and(|live| live.list_done) {
                    self.refresh_channels().await;
                } else {
                    let seen = self.load_channels().await;
                    if self.live.as_ref().is_some_and(|live| live.list_done) {
                        self.drop_left_channels(&seen);
                    }
                }
                emit_chat_list_loaded(&self.events, ProtocolId::Slack);
            }
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
            AdapterCommand::ResendMessage {
                protocol: ProtocolId::Slack,
                conversation_id,
                message_id,
                request,
            } => self.resend(&conversation_id, &message_id, request).await,
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
            self.account(AccountState::Linked);
            self.status(AdapterStatus::Ready, detail);
            return;
        }
        self.account(AccountState::Linking);
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
            self.fail_install_start("Slack app credentials are missing in this build.");
            return;
        };
        let Ok(listener) = SlackLoopback::bind(self.deps.callback_addr).await else {
            self.fail_install_start(DETAIL_PORT_BUSY);
            return;
        };
        let state = new_oauth_state();
        self.deps
            .vault
            .set_secret(SlackSecretKey::OAuthState, &state);
        if !self.deps.browser.open(&authorize_url(&client_id, &state)) {
            self.deps.vault.set_secret(SlackSecretKey::OAuthState, "");
            self.fail_install_start(DETAIL_NO_BROWSER);
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

    fn fail_install_start(&mut self, detail: &str) {
        self.account(AccountState::Unlinked);
        self.status(AdapterStatus::Error, detail);
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
                emit_flush_secrets(&self.events);
                tracing::info!(
                    team_id = grant.workspace.team_id(),
                    "slack workspace app installed"
                );
                self.go_live(grant.bot_token).await;
            }
            Err(InstallFailure::TimedOut) => {
                self.account(AccountState::Unlinked);
                self.status(AdapterStatus::Error, DETAIL_TIMED_OUT);
            }
            Err(InstallFailure::Callback(SlackCallbackError::Declined)) => {
                self.account(AccountState::Unlinked);
                self.status(AdapterStatus::Error, DETAIL_DECLINED);
            }
            Err(InstallFailure::Callback(error)) => {
                self.account(AccountState::Unlinked);
                self.status(AdapterStatus::Error, error.to_string());
            }
            Err(InstallFailure::Exchange(error)) => {
                self.account(AccountState::Unlinked);
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
                if self.live.is_none() {
                    self.account(AccountState::Unlinked);
                }
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
            list_seen: HashSet::new(),
            read_only: HashSet::new(),
            workspace_denied: false,
            policy_writable: HashSet::new(),
        });
        // Linked before any inbox row. A later Linking is only a reconnect.
        self.account(AccountState::Linked);
        self.start_events().await;
        let live_events = self.live.as_ref().is_some_and(|live| live.stream.is_some());
        self.status(
            AdapterStatus::Ready,
            ready_detail(identity.team_name(), live_events),
        );
        let seen = self.load_channels().await;
        if self.live.as_ref().is_some_and(|live| live.list_done) {
            self.drop_left_channels(&seen);
        }
    }

    /// Refresh on a live workspace. Reset the cursor, walk `conversations.list`
    /// again, and drop a channel the app is no longer in once the walk finishes.
    async fn refresh_channels(&mut self) {
        if let Some(live) = &mut self.live {
            live.list_done = false;
            live.next_cursor = None;
            live.list_seen.clear();
        }
        let seen = self.load_channels().await;
        if self.live.as_ref().is_some_and(|live| live.list_done) {
            self.drop_left_channels(&seen);
        }
        let Some(live) = &self.live else {
            return;
        };
        let detail = ready_detail(&live.team_name, live.stream.is_some());
        self.status(AdapterStatus::Ready, detail);
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
        self.shown.clear();
        self.deduped.clear();
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
        emit_flush_secrets(&self.events);
        self.account(AccountState::Unlinked);
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
            emit_notice(&self.events, ProtocolId::Slack, DETAIL_NOT_IN_CHANNEL);
            return;
        }
        emit_command_failed(&self.events, ProtocolId::Slack, None, format!("{error}."));
    }

    fn accumulated_seen(&self) -> HashSet<String> {
        self.live
            .as_ref()
            .map(|live| live.list_seen.clone())
            .unwrap_or_default()
    }

    fn drop_left_channels(&mut self, seen: &HashSet<String>) {
        let gone: Vec<String> = self
            .channels
            .keys()
            .filter(|id| !seen.contains(*id))
            .cloned()
            .collect();
        for id in gone {
            self.shown.remove(&id);
            self.deduped.remove(&id);
            // A rejoin reads `can_post` again. A denial for a channel the
            // app has left must not stick.
            if let Some(live) = &mut self.live {
                live.read_only.remove(&id);
                live.policy_writable.remove(&id);
            }
            if let Some(row) = self.channels.remove(&id) {
                emit_conversation_removed(&self.events, ProtocolId::Slack, row.id);
            }
        }
    }

    async fn load_channels(&mut self) -> HashSet<String> {
        let Some(live) = &self.live else {
            emit_command_failed(&self.events, ProtocolId::Slack, None, DETAIL_NOT_READY);
            return self.accumulated_seen();
        };
        if live.list_done {
            return self.accumulated_seen();
        }
        let token = live.token.clone();
        for _ in 0..MAX_CHANNEL_PAGES {
            if self.live.as_ref().is_none_or(|live| live.list_done) {
                return self.accumulated_seen();
            }
            let cursor = self.live.as_ref().and_then(|live| live.next_cursor.clone());
            let page = match self.deps.api.list_channels(&token, cursor).await {
                Ok(page) => page,
                Err(error) => {
                    self.api_failed(&error).await;
                    return self.accumulated_seen();
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
                if let Some(live) = &mut self.live {
                    live.list_seen.insert(channel.id.clone());
                }
                let title = self.channel_title(&token, &channel).await;
                let (denied, workspace) = self
                    .live
                    .as_ref()
                    .map(|live| (live.read_only.contains(&channel.id), live.workspace_denied))
                    .unwrap_or((false, false));
                if workspace {
                    // `can_post` is the list flag, not `restricted_action`. A
                    // channel that shows up while the policy is on must be
                    // restored with the snapshot when the policy lifts.
                    self.note_writable_under_denial(&channel.id, channel.can_post && !denied);
                }
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
                    writable: channel.can_post && !denied && !workspace,
                    muted: false,
                    placeholder: false,
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
        self.accumulated_seen()
    }

    /// Set the inbox row from the history page. An empty authoritative page clears it.
    fn replace_preview(&mut self, channel: &str, newest: Option<(i64, i64, String)>) {
        let Some(row) = self.channels.get(channel) else {
            return;
        };
        let mut row = row.clone();
        match newest {
            Some((order, sent_at, preview)) => {
                row.preview = preview;
                row.order = order;
                row.last_at = sent_at;
            }
            None => {
                row.preview.clear();
                row.order = 0;
                row.last_at = 0;
            }
        }
        self.upsert(channel.to_string(), row);
    }

    fn note_latest(&mut self, channel: &str, order: i64, sent_at: i64, preview: &str) {
        let Some(row) = self.channels.get(channel) else {
            return;
        };
        if order < row.order {
            return;
        }
        let mut row = row.clone();
        row.preview = preview.to_string();
        row.order = order;
        row.last_at = sent_at;
        self.upsert(channel.to_string(), row);
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
        let Some(channel) = channel_id(conversation).map(str::to_string) else {
            emit_command_failed(
                &self.events,
                ProtocolId::Slack,
                Some(conversation.to_string()),
                "Unknown Slack conversation.",
            );
            return;
        };
        let Some(live) = &self.live else {
            emit_command_failed(
                &self.events,
                ProtocolId::Slack,
                Some(conversation.to_string()),
                DETAIL_NOT_READY,
            );
            return;
        };
        let token = live.token.clone();
        self.recheck_posting(&token, &channel).await;
        let page = match self.deps.api.history(&token, &channel, HISTORY_LIMIT).await {
            Ok(page) => page,
            Err(error) => {
                self.api_failed(&error).await;
                emit_history_loaded(&self.events, ProtocolId::Slack, conversation);
                return;
            }
        };
        // `conversations.history` is newest-first. A channel with no Socket
        // Mode event still needs that post on the inbox row.
        let newest = page
            .posts
            .iter()
            .max_by_key(|post| ts_rank(&post.ts))
            .map(|post| (ts_rank(&post.ts), ts_order(&post.ts), post.text.clone()));
        // Ignored subtypes count toward Slack's raw limit, so a short
        // displayable page is not the latest window. Drop only the posts
        // that page covers, before `remember` can evict them without
        // `MessagesRemoved`.
        let keep_cached_preview = page.posts.is_empty() && !page.authoritative;
        self.drop_posts_outside(&channel, &page);
        for post in page.posts.into_iter().rev() {
            // A history page refills `shown` even when the identity is already
            // deduped. A Socket Mode retry does not; see `inbound`.
            if let Some(message) = self.chat_message(&token, post, true).await {
                emit_message(&self.events, message);
            }
        }
        // A dropped live post can be newer than every history row.
        // `note_latest` would keep that stale preview. An empty page that
        // is not the end of the channel uses a remaining cached post.
        // No cached post means the preview pointed at a removed message.
        if keep_cached_preview {
            self.replace_preview(&channel, self.newest_shown(&channel));
        } else {
            self.replace_preview(&channel, newest);
        }
        self.mark_channel_read(&channel);
        emit_history_loaded(&self.events, ProtocolId::Slack, conversation);
    }

    fn set_view(&mut self, conversation_id: Option<String>) {
        let channel = conversation_id
            .as_deref()
            .and_then(channel_id)
            .map(str::to_string);
        self.viewing.clone_from(&channel);
        let Some(channel) = channel else {
            return;
        };
        self.mark_channel_read(&channel);
    }

    async fn resend(&mut self, conversation: &str, message_id: &str, request: u64) {
        let text = channel_id(conversation).and_then(|channel| {
            self.shown.get(channel).and_then(|rows| {
                rows.iter().find_map(|row| {
                    (message_id == message_id_of(channel, &row.ts)).then(|| row.text.clone())
                })
            })
        });
        let Some(text) = text else {
            emit_send_rejected(&self.events, ProtocolId::Slack, conversation, request);
            return;
        };
        self.send(conversation, &text, request).await;
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
                self.clear_posting_denial(channel);
                let order = ts_rank(&post.ts);
                let sent_at = ts_order(&post.ts);
                let Some(message) = self.chat_message(&token, post, true).await else {
                    // Slack accepted a post older than the cached window.
                    // The send still counts; the row is not shown.
                    emit_send_accepted(&self.events, ProtocolId::Slack, conversation, request);
                    return;
                };
                let preview = message.body.clone();
                emit_send_accepted(
                    &self.events,
                    ProtocolId::Slack,
                    message.conversation_id.clone(),
                    request,
                );
                emit_message(&self.events, message);
                // No Socket Mode echo when the app-level token is absent.
                // The post response is the only update the inbox will see.
                self.note_latest(channel, order, sent_at, &preview);
            }
            Err(error) => {
                if workspace_policy(&error) {
                    self.mark_workspace_read_only();
                } else if posting_denied(&error) {
                    self.mark_read_only(channel);
                }
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
            message_id_of(channel, ts),
            text,
        );
        if let Some(rows) = self.shown.get_mut(channel)
            && let Some(row) = rows.iter_mut().find(|row| row.ts == ts)
        {
            row.text = text.to_string();
        }
        let Some(row) = self.channels.get(channel) else {
            return;
        };
        if ts_rank(ts) != row.order || row.order == 0 {
            return;
        }
        let mut row = row.clone();
        row.preview = text.to_string();
        self.upsert(channel.to_string(), row);
    }

    fn delete_message(&mut self, channel: &str, ts: &str) {
        emit_messages_removed(
            &self.events,
            ProtocolId::Slack,
            conversation_id(channel),
            vec![message_id_of(channel, ts)],
        );
        let counted = self.drop_shown(channel, ts);
        let Some(row) = self.channels.get(channel) else {
            return;
        };
        let latest = ts_rank(ts) == row.order && row.order != 0;
        if !latest && !counted {
            return;
        }
        let next = self.shown.get(channel).and_then(|rows| {
            rows.iter()
                .max_by_key(|row| ts_rank(&row.ts))
                .map(|row| (row.ts.clone(), row.text.clone()))
        });
        let mut row = row.clone();
        if counted {
            row.unread = row.unread.saturating_sub(1);
        }
        if !latest {
            self.upsert(channel.to_string(), row);
            return;
        }
        match next {
            Some((next_ts, text)) => {
                row.preview = text;
                row.order = ts_rank(&next_ts);
                row.last_at = ts_order(&next_ts);
            }
            None => {
                row.preview.clear();
                row.order = 0;
                row.last_at = 0;
            }
        }
        self.upsert(channel.to_string(), row);
    }

    async fn inbound(&mut self, post: SlackPost) {
        let Some(live) = &self.live else {
            return;
        };
        let token = live.token.clone();
        let channel = post.channel.clone();
        let post_ts = post.ts.clone();
        let duplicate = self.already_seen(&post);
        let order = ts_rank(&post.ts);
        let sent_at = ts_order(&post.ts);
        // A Socket Mode post is live the first time only. A retry, or a post
        // this session already accepted, stays `History` so it cannot notify
        // (#32). `deduped` keeps that identity after `shown` evicts it, for
        // the notification freshness window.
        let arrival = if duplicate {
            crate::Arrival::History
        } else {
            crate::Arrival::Live
        };
        // Putting an evicted identity back on `shown` would push out a newer
        // row and drop its unread count. Update the display cache only when
        // the post is new or still on screen.
        let keep_display = !duplicate || self.displayed(&post);
        let Some(built) = self.chat_message(&token, post, keep_display).await else {
            // Older than every cached row. `remember` already dropped it
            // without `MessagesRemoved`, so the thread stays at the cap.
            return;
        };
        let message = ChatMessage { arrival, ..built };
        let preview = message.body.clone();
        let outbound = message.outbound;
        let sender = message.sender.clone();
        let Some(mut row) = self.channels.get(&channel).cloned() else {
            // A new DM or a channel the bot just joined. `conversations.list` fills the
            // real title on the next load; until then a DM shows the sender.
            let title = if channel.starts_with('D') {
                sender
            } else {
                format!("#{channel}")
            };
            let workspace_denied = self.live.as_ref().is_some_and(|live| live.workspace_denied);
            let conversation = Conversation {
                protocol: ProtocolId::Slack,
                id: conversation_id(&channel),
                title,
                participant: "workspace".into(),
                preview,
                unread: u32::from(
                    !duplicate && !outbound && self.viewing.as_deref() != Some(channel.as_str()),
                ),
                order,
                last_at: sent_at,
                is_group: !channel.starts_with('D'),
                writable: !workspace_denied,
                muted: false,
                placeholder: false,
            };
            if workspace_denied {
                let would_post = self
                    .live
                    .as_ref()
                    .is_none_or(|live| !live.read_only.contains(&channel));
                self.note_writable_under_denial(&channel, would_post);
            }
            // The title is only the id. A later LoadChats walks the list again
            // and replaces it with the channel name.
            if let Some(live) = &mut self.live {
                live.list_done = false;
                live.next_cursor = None;
                live.list_seen.clear();
            }
            if conversation.unread > 0 {
                self.count_unread(&channel, &post_ts);
            }
            // Core skips a live message whose chat is missing (`UnknownChat`)
            // and does not reconsider it. The row goes out first, so this
            // first post can notify. A repeat stays `History` (#160 review).
            self.upsert(channel, conversation);
            emit_message(&self.events, message);
            return;
        };
        emit_message(&self.events, message);
        if order >= row.order {
            row.preview = preview;
            row.order = order;
            row.last_at = sent_at;
        }
        if !duplicate && !outbound && self.viewing.as_deref() != Some(channel.as_str()) {
            row.unread = row.unread.saturating_add(1);
            self.count_unread(&channel, &post_ts);
        }
        self.upsert(channel, row);
    }

    fn drop_shown(&mut self, channel: &str, ts: &str) -> bool {
        let Some(rows) = self.shown.get_mut(channel) else {
            return false;
        };
        let counted = rows.iter().any(|row| row.ts == ts && row.counted);
        rows.retain(|row| row.ts != ts);
        counted
    }

    fn count_unread(&mut self, channel: &str, ts: &str) {
        if let Some(rows) = self.shown.get_mut(channel)
            && let Some(row) = rows.iter_mut().find(|row| row.ts == ts)
        {
            row.counted = true;
        }
    }

    fn mark_channel_read(&mut self, channel: &str) {
        if let Some(rows) = self.shown.get_mut(channel) {
            for row in rows {
                row.counted = false;
            }
        }
        let Some(row) = self.channels.get(channel) else {
            return;
        };
        if row.unread == 0 {
            return;
        }
        let mut row = row.clone();
        row.unread = 0;
        self.upsert(channel.to_string(), row);
    }

    fn already_seen(&self, post: &SlackPost) -> bool {
        // Still on screen, or kept past display eviction for the freshness
        // window. Either one is enough; liveness does not follow `shown` alone.
        self.displayed(post)
            || self.deduped.get(&post.channel).is_some_and(|rows| {
                rows.iter()
                    .any(|row| same_post(&row.ts, row.client_msg_id.as_deref(), post))
            })
    }

    fn displayed(&self, post: &SlackPost) -> bool {
        self.shown.get(&post.channel).is_some_and(|rows| {
            rows.iter()
                .any(|row| same_post(&row.ts, row.client_msg_id.as_deref(), post))
        })
    }

    /// Records `post` for liveness. A repeat keeps the first timestamp so age
    /// eviction follows the original post, and fills in `client_msg_id` when
    /// the first sight of it had none.
    fn note_seen(&mut self, post: &SlackPost) {
        let rows = self.deduped.entry(post.channel.clone()).or_default();
        if let Some(row) = rows
            .iter_mut()
            .find(|row| same_post(&row.ts, row.client_msg_id.as_deref(), post))
        {
            if row.client_msg_id.is_none() {
                row.client_msg_id.clone_from(&post.client_msg_id);
            }
            return;
        }
        rows.push(Deduped {
            ts: post.ts.clone(),
            client_msg_id: post.client_msg_id.clone(),
            sent_at: ts_order(&post.ts),
        });
        prune_deduped(rows);
    }

    /// Returns false when `post` is older than the kept window and was not shown.
    /// A kept post is also recorded in `deduped`, so a Socket Mode retry stays
    /// `History` after `shown` evicts it. A post this window refuses is not
    /// recorded: a retry must hit the same refusal instead of being shown.
    fn remember(&mut self, post: &SlackPost) -> bool {
        let dropped = {
            let rows = self.shown.entry(post.channel.clone()).or_default();
            if let Some(row) = rows
                .iter_mut()
                .find(|row| same_post(&row.ts, row.client_msg_id.as_deref(), post))
            {
                row.text.clone_from(&post.text);
                if row.client_msg_id.is_none() {
                    row.client_msg_id.clone_from(&post.client_msg_id);
                }
                None
            } else {
                rows.push(Shown {
                    ts: post.ts.clone(),
                    text: post.text.clone(),
                    counted: false,
                    client_msg_id: post.client_msg_id.clone(),
                });
                let cap = usize::from(HISTORY_LIMIT);
                Some(if rows.len() > cap {
                    // A live post can sit at index 0. History then appends older
                    // rows. The cap drops the oldest `ts`, not the first insert.
                    take_oldest(rows, rows.len() - cap)
                } else {
                    Vec::new()
                })
            }
        };
        let Some(dropped) = dropped else {
            self.note_seen(post);
            return true;
        };
        // The post just cached can be the oldest row. Do not emit
        // `MessagesRemoved` for it: the UI has not seen it, and core applies
        // the removal before the insert, so the row stays and the thread
        // passes the cap.
        let kept = !dropped.iter().any(|row| row.ts == post.ts);
        let dropped: Vec<Shown> = if kept {
            dropped
        } else {
            dropped
                .into_iter()
                .filter(|row| row.ts != post.ts)
                .collect()
        };
        if !dropped.is_empty() {
            let lost = dropped.iter().filter(|row| row.counted).count() as u32;
            let ids = dropped
                .iter()
                .map(|row| message_id_of(&post.channel, &row.ts))
                .collect();
            emit_messages_removed(
                &self.events,
                ProtocolId::Slack,
                conversation_id(&post.channel),
                ids,
            );
            if lost > 0 {
                self.forget_unread(&post.channel, lost);
            }
        }
        if kept {
            self.note_seen(post);
        }
        kept
    }

    fn forget_unread(&mut self, channel: &str, lost: u32) {
        let Some(row) = self.channels.get(channel) else {
            return;
        };
        let mut row = row.clone();
        row.unread = row.unread.saturating_sub(lost);
        self.upsert(channel.to_string(), row);
    }

    fn mark_read_only(&mut self, channel: &str) {
        if let Some(live) = &mut self.live {
            live.read_only.insert(channel.to_string());
        }
        self.force_unwritable(channel);
    }

    /// `restricted_action` applies to every row. A later allow restores the
    /// rows that were writable when the policy arrived, plus channels learned
    /// during the denial that Slack would allow.
    fn mark_workspace_read_only(&mut self) {
        let writable: Vec<String> = self
            .channels
            .iter()
            .filter(|(_, row)| row.writable)
            .map(|(id, _)| id.clone())
            .collect();
        let ids: Vec<String> = self.channels.keys().cloned().collect();
        if let Some(live) = &mut self.live {
            if !live.workspace_denied {
                live.policy_writable = writable.into_iter().collect();
            }
            live.workspace_denied = true;
        }
        for id in ids {
            self.force_unwritable(&id);
        }
    }

    fn force_unwritable(&mut self, channel: &str) {
        let Some(row) = self.channels.get(channel) else {
            return;
        };
        if !row.writable {
            return;
        }
        let mut row = row.clone();
        row.writable = false;
        self.upsert(channel.to_string(), row);
    }

    fn set_writable(&mut self, channel: &str, writable: bool) {
        let Some(row) = self.channels.get(channel) else {
            return;
        };
        if row.writable == writable {
            return;
        }
        let mut row = row.clone();
        row.writable = writable;
        self.upsert(channel.to_string(), row);
    }

    /// A successful post, or `conversations.info` that allows one, drops the
    /// refusal. A workspace policy restores every row it turned off.
    ///
    /// `restricted_action` is treated as workspace policy, so one channel
    /// clearing it lifts the denial everywhere. A successful post is the
    /// strong signal. Channel-level `can_post` may not fully reflect that
    /// policy.
    fn clear_posting_denial(&mut self, channel: &str) {
        let (workspace, restore) = {
            let Some(live) = &mut self.live else {
                return;
            };
            live.read_only.remove(channel);
            let workspace = live.workspace_denied;
            live.workspace_denied = false;
            let restore = if workspace {
                live.policy_writable.drain().collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            (workspace, restore)
        };
        if workspace {
            for id in restore {
                let denied = self
                    .live
                    .as_ref()
                    .is_some_and(|live| live.read_only.contains(&id));
                if !denied {
                    self.set_writable(&id, true);
                }
            }
        }
        let denied = self
            .live
            .as_ref()
            .is_some_and(|live| live.read_only.contains(channel));
        if !denied {
            self.set_writable(channel, true);
        }
    }

    async fn recheck_posting(&mut self, token: &SlackBotToken, channel: &str) {
        let denied = self
            .live
            .as_ref()
            .is_some_and(|live| live.workspace_denied || live.read_only.contains(channel));
        if !denied {
            return;
        }
        match self.deps.api.posting_allowed(token, channel).await {
            Ok(true) => self.clear_posting_denial(channel),
            Ok(false) => self.mark_read_only(channel),
            Err(error) => {
                tracing::debug!(
                    %error,
                    channel,
                    "slack posting recheck failed; composer stays disabled"
                );
            }
        }
    }

    /// Record whether `channel` should be writable once a workspace denial lifts.
    ///
    /// `would_post` is `can_post && !read_only` from the channel list. A
    /// socket-mode channel has no `can_post` yet; it is recorded unless a
    /// channel-level refusal is already set.
    fn note_writable_under_denial(&mut self, channel: &str, would_post: bool) {
        let Some(live) = &mut self.live else {
            return;
        };
        if !live.workspace_denied {
            return;
        }
        if would_post {
            live.policy_writable.insert(channel.to_string());
        } else {
            live.policy_writable.remove(channel);
        }
    }

    fn newest_shown(&self, channel: &str) -> Option<(i64, i64, String)> {
        self.shown.get(channel).and_then(|rows| {
            rows.iter()
                .max_by_key(|row| ts_rank(&row.ts))
                .map(|row| (ts_rank(&row.ts), ts_order(&row.ts), row.text.clone()))
        })
    }

    fn drop_posts_outside(&mut self, channel: &str, page: &SlackHistoryPage) {
        let kept: HashSet<&str> = page.posts.iter().map(|post| post.ts.as_str()).collect();
        let oldest_raw = page.oldest_raw_ts.as_deref().map(ts_rank);
        let Some(rows) = self.shown.get_mut(channel) else {
            return;
        };
        let gone: Vec<String> = rows
            .iter()
            .filter(|row| cached_post_is_outside(&row.ts, &kept, oldest_raw, page.authoritative))
            .map(|row| row.ts.clone())
            .collect();
        if gone.is_empty() {
            return;
        }
        rows.retain(|row| !gone.contains(&row.ts));
        let ids = gone.iter().map(|ts| message_id_of(channel, ts)).collect();
        emit_messages_removed(
            &self.events,
            ProtocolId::Slack,
            conversation_id(channel),
            ids,
        );
    }

    async fn chat_message(
        &mut self,
        token: &SlackBotToken,
        post: SlackPost,
        keep_display: bool,
    ) -> Option<ChatMessage> {
        if keep_display && !self.remember(&post) {
            return None;
        }
        let outbound = self
            .live
            .as_ref()
            .is_some_and(|live| post.user.as_deref() == Some(live.bot_user_id.as_str()));
        let sender = match (&post.user, &post.username) {
            (_, Some(username)) if !username.trim().is_empty() => username.clone(),
            (Some(user), _) => self.user_name(token, user).await,
            (None, _) => "Slack".into(),
        };
        Some(ChatMessage {
            protocol: ProtocolId::Slack,
            conversation_id: conversation_id(&post.channel),
            id: message_id_of(&post.channel, &post.ts),
            sender,
            body: post.text,
            outbound,
            delivery: Delivery::Sent,
            sent_at: ts_order(&post.ts),
            arrival: crate::Arrival::History,
        })
    }
}

struct Shown {
    ts: String,
    text: String,
    counted: bool,
    client_msg_id: Option<String>,
}

/// One accepted post identity. `sent_at` is the Slack `ts` in unix seconds
/// (`0` when it does not parse). Age eviction uses that, not wall time, so a
/// retry is matched against the original post.
struct Deduped {
    ts: String,
    client_msg_id: Option<String>,
    sent_at: i64,
}

/// Drop identities older than the freshness window, then enforce `DEDUP_LIMIT`.
fn prune_deduped(rows: &mut Vec<Deduped>) {
    let Some(horizon) = rows.iter().map(|row| row.sent_at).max() else {
        return;
    };
    let cutoff = horizon.saturating_sub(DEDUP_FRESH_SECS);
    // `sent_at == 0` is an unparsed timestamp. Notifications still allow it,
    // so keep the identity until the size cap.
    rows.retain(|row| row.sent_at == 0 || row.sent_at >= cutoff);
    if rows.len() <= DEDUP_LIMIT {
        return;
    }
    rows.sort_by_key(|row| row.sent_at);
    let extra = rows.len() - DEDUP_LIMIT;
    rows.drain(0..extra);
}

/// A cached post leaves the thread when this page covers its timestamp and
/// does not include it. A non-authoritative page covers only `ts >= oldest_raw`.
fn cached_post_is_outside(
    ts: &str,
    kept: &HashSet<&str>,
    oldest_raw: Option<i64>,
    authoritative: bool,
) -> bool {
    if kept.contains(ts) {
        return false;
    }
    if authoritative {
        return true;
    }
    oldest_raw.is_some_and(|oldest| ts_rank(ts) >= oldest)
}

fn same_client_msg(stored: Option<&str>, incoming: Option<&str>) -> bool {
    match (stored, incoming) {
        (Some(stored), Some(incoming)) => !stored.is_empty() && stored == incoming,
        _ => false,
    }
}

fn same_post(ts: &str, client_msg_id: Option<&str>, post: &SlackPost) -> bool {
    ts == post.ts || same_client_msg(client_msg_id, post.client_msg_id.as_deref())
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
fn message_id_of(channel: &str, ts: &str) -> String {
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

/// Slack refused a top-level post in this channel. A transport error is not a refusal.
fn posting_denied(error: &SlackApiError) -> bool {
    matches!(
        error,
        SlackApiError::Api(code) if matches!(
            code.as_str(),
            "not_in_channel"
                | "is_archived"
                | "restricted_action_read_only_channel"
                | "restricted_action_thread_only_channel"
        )
    )
}

/// `restricted_action` is a workspace policy. The channel-specific codes are not.
fn workspace_policy(error: &SlackApiError) -> bool {
    matches!(error, SlackApiError::Api(code) if code == "restricted_action")
}

/// Drop the `extra` rows with the smallest `ts_rank`.
fn take_oldest(rows: &mut Vec<Shown>, extra: usize) -> Vec<Shown> {
    let mut ranked: Vec<usize> = (0..rows.len()).collect();
    ranked.sort_by_key(|&index| (ts_rank(&rows[index].ts), index));
    let mut victims: Vec<usize> = ranked.into_iter().take(extra).collect();
    victims.sort_unstable_by(|left, right| right.cmp(left));
    victims
        .into_iter()
        .map(|index| rows.remove(index))
        .collect()
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
    use std::collections::HashSet;

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
            message_id_of("C1", "1700000001.000100"),
            "slack:C1:1700000001000100"
        );
        assert_eq!(ts_rank("bad"), 0);
    }

    #[test]
    fn a_post_older_than_a_diluted_page_stays() {
        let kept = HashSet::new();
        let oldest = ts_rank("1700000003.000100");
        assert!(!cached_post_is_outside(
            "1690000000.000100",
            &kept,
            Some(oldest),
            false
        ));
        assert!(cached_post_is_outside(
            "1700000004.000100",
            &kept,
            Some(oldest),
            false
        ));
        assert!(!cached_post_is_outside(
            "1700000004.000100",
            &HashSet::from(["1700000004.000100"]),
            Some(oldest),
            false
        ));
        assert!(cached_post_is_outside(
            "1690000000.000100",
            &kept,
            Some(oldest),
            true
        ));
        assert!(!cached_post_is_outside(
            "1690000000.000100",
            &kept,
            None,
            false
        ));
    }

    #[test]
    fn take_oldest_drops_the_incoming_post_when_it_is_the_oldest() {
        let mut rows: Vec<Shown> = (1..=50)
            .map(|index| Shown {
                ts: format!("170000{index:04}.000100"),
                text: format!("n{index}"),
                counted: false,
                client_msg_id: None,
            })
            .collect();
        rows.push(Shown {
            ts: "1600000000.000100".into(),
            text: "too old".into(),
            counted: false,
            client_msg_id: None,
        });
        let dropped = take_oldest(&mut rows, 1);
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].ts, "1600000000.000100");
        assert_eq!(rows.len(), 50);
        assert!(rows.iter().all(|row| row.ts != "1600000000.000100"));
    }

    #[test]
    fn deduped_ids_outlive_the_display_cache_inside_the_freshness_window() {
        let start = 1_700_000_000;
        let mut rows = Vec::new();
        for offset in 0..=i64::from(HISTORY_LIMIT) {
            rows.push(Deduped {
                ts: offset.to_string(),
                client_msg_id: None,
                sent_at: start + offset,
            });
        }
        assert!(i64::from(HISTORY_LIMIT) < DEDUP_FRESH_SECS);
        prune_deduped(&mut rows);
        assert_eq!(rows.len(), usize::from(HISTORY_LIMIT) + 1);
        assert!(rows.iter().any(|row| row.ts == "0"));

        let horizon = start + i64::from(HISTORY_LIMIT);
        rows.push(Deduped {
            ts: "stale".into(),
            client_msg_id: Some("stale-id".into()),
            sent_at: horizon - DEDUP_FRESH_SECS - 1,
        });
        prune_deduped(&mut rows);
        assert!(rows.iter().all(|row| row.ts != "stale"));
        assert!(rows.iter().any(|row| row.ts == "0"));
    }

    #[test]
    fn a_post_at_the_freshness_edge_stays_deduped() {
        let horizon = 1_700_000_500;
        let mut rows = vec![
            Deduped {
                ts: "edge".into(),
                client_msg_id: None,
                sent_at: horizon - DEDUP_FRESH_SECS,
            },
            Deduped {
                ts: "newest".into(),
                client_msg_id: None,
                sent_at: horizon,
            },
            Deduped {
                ts: "unknown".into(),
                client_msg_id: None,
                sent_at: 0,
            },
        ];
        prune_deduped(&mut rows);
        let ts: Vec<_> = rows.iter().map(|row| row.ts.as_str()).collect();
        assert!(ts.contains(&"edge"));
        assert!(ts.contains(&"newest"));
        assert!(ts.contains(&"unknown"));
    }

    #[test]
    fn deduped_ids_stay_bounded_inside_the_freshness_window() {
        let mut rows = Vec::new();
        for index in 0..=DEDUP_LIMIT {
            rows.push(Deduped {
                ts: index.to_string(),
                client_msg_id: None,
                sent_at: 1_700_000_000 + i64::try_from(index % 60).expect("second"),
            });
        }
        prune_deduped(&mut rows);
        assert_eq!(rows.len(), DEDUP_LIMIT);
    }

    #[test]
    fn same_post_matches_ts_or_client_msg_id() {
        let post = SlackPost {
            channel: "C1".into(),
            ts: "1700000001.000200".into(),
            user: None,
            username: None,
            text: "x".into(),
            client_msg_id: Some("client-1".into()),
        };
        assert!(same_post("1700000001.000100", Some("client-1"), &post));
        assert!(same_post("1700000001.000200", None, &post));
        assert!(!same_post("1700000001.000100", Some("other"), &post));
        assert!(!same_post("1700000001.000100", Some(""), &post));
        assert!(!same_post("nope", None, &post));
    }
}
