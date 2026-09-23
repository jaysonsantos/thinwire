//! In-process Slack fakes and inbox tests. No network except the loopback
//! OAuth redirect, which the test drives like a browser would.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use super::api::{
    SlackApiError, SlackAppToken, SlackBotToken, SlackBrowser, SlackChannel, SlackChannelKind,
    SlackChannelPage, SlackCodeExchange, SlackEventSource, SlackEventStream, SlackInbound,
    SlackInstallGrant, SlackPost, SlackWebApi,
};
use super::credentials::SlackApiSource;
use super::install::SlackInstalledWorkspace;
use super::loopback::tests::get;
use super::secrets::{MemorySlackVault, SlackSecretKey, SlackSecretVault};
use super::session::{SlackDeps, SlackInbox};
use crate::adapter::{
    AdapterCommand, AdapterEvent, AdapterStatus, ChatMessage, Conversation, ProtocolAdapter,
    ProtocolId,
};

const BOT_TOKEN: &str = "xoxb-fake-bot-token";
const APP_TOKEN: &str = "xapp-fake-app-token";
const CLIENT_ID: &str = "client-id-fake";
const CLIENT_SECRET: &str = "client-secret-fake";
const CODE: &str = "oauth-code-fake";
const BOT_USER: &str = "U0BOT";
const SECRETS: &[&str] = &[BOT_TOKEN, APP_TOKEN, CLIENT_SECRET, CODE];

#[derive(Default)]
struct ApiState {
    calls: Vec<String>,
    exchanged: Vec<SlackCodeExchange>,
    identify_error: Option<SlackApiError>,
    pages: Vec<SlackChannelPage>,
    history: HashMap<String, Vec<SlackPost>>,
    history_error: Option<SlackApiError>,
    users: HashMap<String, String>,
    posted: Vec<(String, String)>,
    next_ts: u64,
}

#[derive(Default)]
struct FakeApi {
    state: Mutex<ApiState>,
}

impl FakeApi {
    fn with<R>(&self, apply: impl FnOnce(&mut ApiState) -> R) -> R {
        apply(&mut self.state.lock().expect("fake api"))
    }

    fn check_token(token: &SlackBotToken) {
        assert_eq!(token.reveal(), BOT_TOKEN, "only the workspace bot token");
    }

    fn workspace() -> Self {
        let api = Self::default();
        api.with(|state| {
            state.pages = vec![
                SlackChannelPage {
                    channels: vec![
                        channel("C1", "general", SlackChannelKind::Public, true),
                        channel("C2", "random", SlackChannelKind::Public, false),
                    ],
                    next_cursor: Some("page-2".into()),
                },
                SlackChannelPage {
                    channels: vec![
                        SlackChannel {
                            dm_user: Some("U1".into()),
                            ..channel("D1", "", SlackChannelKind::DirectMessage, true)
                        },
                        channel("G1", "mpdm-ana--bo-1", SlackChannelKind::GroupMessage, true),
                    ],
                    next_cursor: None,
                },
            ];
            state.users.insert("U1".into(), "Ana".into());
            state.users.insert(BOT_USER.into(), "thinwire".into());
            state.history.insert(
                "C1".into(),
                vec![
                    post("C1", "1700000002.000200", BOT_USER, "second, from the app"),
                    post("C1", "1700000001.000100", "U1", "first"),
                ],
            );
            state.next_ts = 1_700_000_100;
        });
        api
    }
}

fn channel(id: &str, name: &str, kind: SlackChannelKind, is_member: bool) -> SlackChannel {
    SlackChannel {
        id: id.into(),
        name: name.into(),
        kind,
        is_member,
        dm_user: None,
    }
}

fn post(channel: &str, ts: &str, user: &str, text: &str) -> SlackPost {
    SlackPost {
        channel: channel.into(),
        ts: ts.into(),
        user: Some(user.into()),
        username: None,
        text: text.into(),
    }
}

impl SlackWebApi for FakeApi {
    async fn identify(
        &self,
        token: &SlackBotToken,
    ) -> Result<SlackInstalledWorkspace, SlackApiError> {
        Self::check_token(token);
        self.with(|state| {
            state.calls.push("auth.test".into());
            match state.identify_error.clone() {
                Some(error) => Err(error),
                None => Ok(SlackInstalledWorkspace::new("T1", "Fake Co", BOT_USER, "")),
            }
        })
    }

    async fn exchange_code(
        &self,
        exchange: SlackCodeExchange,
    ) -> Result<SlackInstallGrant, SlackApiError> {
        self.with(|state| {
            state.calls.push("oauth.v2.access".into());
            state.exchanged.push(exchange);
        });
        Ok(SlackInstallGrant {
            workspace: SlackInstalledWorkspace::new("T1", "Fake Co", BOT_USER, "A1"),
            bot_token: SlackBotToken::new(BOT_TOKEN),
        })
    }

    async fn list_channels(
        &self,
        token: &SlackBotToken,
        cursor: Option<String>,
    ) -> Result<SlackChannelPage, SlackApiError> {
        Self::check_token(token);
        self.with(|state| {
            state.calls.push(format!(
                "conversations.list {}",
                cursor.as_deref().unwrap_or("-")
            ));
            let index = usize::from(cursor.is_some());
            Ok(state.pages.get(index).cloned().unwrap_or_default())
        })
    }

    async fn history(
        &self,
        token: &SlackBotToken,
        channel: &str,
        limit: u16,
    ) -> Result<Vec<SlackPost>, SlackApiError> {
        Self::check_token(token);
        assert!(limit > 0);
        self.with(|state| {
            state.calls.push(format!("conversations.history {channel}"));
            if let Some(error) = state.history_error.clone() {
                return Err(error);
            }
            Ok(state.history.get(channel).cloned().unwrap_or_default())
        })
    }

    async fn post_message(
        &self,
        token: &SlackBotToken,
        channel: &str,
        text: &str,
    ) -> Result<SlackPost, SlackApiError> {
        Self::check_token(token);
        self.with(|state| {
            state.calls.push(format!("chat.postMessage {channel}"));
            state.posted.push((channel.into(), text.into()));
            state.next_ts += 1;
            Ok(post(
                channel,
                &format!("{}.000100", state.next_ts),
                BOT_USER,
                text,
            ))
        })
    }

    async fn user_name(&self, token: &SlackBotToken, user: &str) -> Result<String, SlackApiError> {
        Self::check_token(token);
        self.with(|state| {
            state.calls.push(format!("users.info {user}"));
            state
                .users
                .get(user)
                .cloned()
                .ok_or_else(|| SlackApiError::api("user_not_found"))
        })
    }
}

#[derive(Default)]
struct FakeSocket {
    sink: Mutex<Option<UnboundedSender<SlackInbound>>>,
    stopped: Arc<AtomicBool>,
}

impl FakeSocket {
    fn push(&self, event: SlackInbound) {
        let sink = self.sink.lock().expect("sink");
        sink.as_ref()
            .expect("socket connected")
            .send(event)
            .expect("inbox alive");
    }

    fn connected(&self) -> bool {
        self.sink.lock().expect("sink").is_some()
    }
}

struct FakeStream {
    stopped: Arc<AtomicBool>,
}

impl SlackEventStream for FakeStream {
    async fn stop(self) {
        self.stopped.store(true, Ordering::SeqCst);
    }
}

impl SlackEventSource for FakeSocket {
    type Stream = FakeStream;

    async fn connect(
        &self,
        app_token: SlackAppToken,
        sink: UnboundedSender<SlackInbound>,
    ) -> Result<FakeStream, SlackApiError> {
        assert_eq!(app_token.reveal(), APP_TOKEN);
        *self.sink.lock().expect("sink") = Some(sink);
        Ok(FakeStream {
            stopped: Arc::clone(&self.stopped),
        })
    }
}

struct FakeBrowser {
    opened: Mutex<Vec<String>>,
    works: bool,
}

impl FakeBrowser {
    fn new(works: bool) -> Self {
        Self {
            opened: Mutex::new(Vec::new()),
            works,
        }
    }

    fn last(&self) -> Option<String> {
        self.opened.lock().expect("browser").last().cloned()
    }
}

impl SlackBrowser for FakeBrowser {
    fn open(&self, url: &str) -> bool {
        self.opened.lock().expect("browser").push(url.into());
        self.works
    }
}

struct Harness {
    adapter: SlackInbox<FakeApi, FakeSocket, FakeBrowser>,
    api: Arc<FakeApi>,
    socket: Arc<FakeSocket>,
    browser: Arc<FakeBrowser>,
    vault: Arc<MemorySlackVault>,
    callback: SocketAddr,
    tx: UnboundedSender<AdapterEvent>,
    rx: UnboundedReceiver<AdapterEvent>,
    seen: Vec<AdapterEvent>,
}

impl Harness {
    fn new(api: FakeApi, vault: MemorySlackVault, browser_works: bool) -> Self {
        let vault = Arc::new(vault);
        let callback = free_loopback();
        let mut deps = SlackDeps::new(
            api,
            FakeSocket::default(),
            FakeBrowser::new(browser_works),
            Arc::clone(&vault) as Arc<dyn SlackSecretVault>,
            SlackApiSource::empty(),
        );
        deps.callback_addr = callback;
        deps.install_timeout = Duration::from_secs(5);
        let api = Arc::clone(&deps.api);
        let socket = Arc::clone(&deps.socket);
        let browser = Arc::clone(&deps.browser);
        let (tx, rx) = unbounded_channel();
        Self {
            adapter: SlackInbox::new(deps),
            api,
            socket,
            browser,
            vault,
            callback,
            tx,
            rx,
            seen: Vec::new(),
        }
    }

    fn start(&mut self) {
        self.adapter.start(self.tx.clone());
    }

    fn send(&mut self, command: AdapterCommand) {
        self.adapter.handle(command, &self.tx).expect("queued");
    }

    /// Wait until an event matches. Keeps every event for later checks.
    async fn until(&mut self, what: &str, test: impl Fn(&AdapterEvent) -> bool) -> AdapterEvent {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let event = tokio::time::timeout_at(deadline, self.rx.recv())
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for {what}; saw {:?}", self.seen))
                .expect("events open");
            assert_no_secrets(&event);
            self.seen.push(event.clone());
            if test(&event) {
                return event;
            }
        }
    }

    async fn status(&mut self, status: AdapterStatus) -> String {
        match self
            .until("status", |event| {
                matches!(event, AdapterEvent::Status { protocol: ProtocolId::Slack, status: seen, .. } if *seen == status)
            })
            .await
        {
            AdapterEvent::Status { detail, .. } => detail,
            _ => unreachable!(),
        }
    }

    async fn conversation(&mut self, id: &str) -> Conversation {
        match self
            .until(id, |event| {
                matches!(event, AdapterEvent::ConversationUpsert { conversation } if conversation.id == id)
            })
            .await
        {
            AdapterEvent::ConversationUpsert { conversation } => conversation,
            _ => unreachable!(),
        }
    }

    async fn message(&mut self, body: &str) -> ChatMessage {
        match self
            .until(body, |event| {
                matches!(event, AdapterEvent::MessageReceived { message } if message.body == body)
            })
            .await
        {
            AdapterEvent::MessageReceived { message } => message,
            _ => unreachable!(),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.api.with(|state| state.calls.clone())
    }
}

fn free_loopback() -> SocketAddr {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("probe");
    probe.local_addr().expect("addr")
}

fn assert_no_secrets(event: &AdapterEvent) {
    let shown = format!("{event:?}");
    for secret in SECRETS {
        assert!(!shown.contains(secret), "event leaked a secret: {shown}");
    }
}

fn installed_vault() -> MemorySlackVault {
    let vault = MemorySlackVault::new();
    vault.set_secret(SlackSecretKey::BotToken, BOT_TOKEN);
    vault.set_secret(SlackSecretKey::TeamId, "T1");
    vault
}

fn client_vault() -> MemorySlackVault {
    let vault = MemorySlackVault::new();
    vault.set_secret(SlackSecretKey::ClientId, CLIENT_ID);
    vault.set_secret(SlackSecretKey::ClientSecret, CLIENT_SECRET);
    vault
}

fn query_value(url: &str, name: &str) -> String {
    let query = url.split_once('?').expect("query").1;
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix(&format!("{name}=")))
        .expect("parameter")
        .to_string()
}

#[tokio::test]
async fn install_exchanges_the_code_stores_the_bot_token_and_lists_channels() {
    let mut h = Harness::new(FakeApi::workspace(), client_vault(), true);
    h.start();
    assert!(
        h.status(AdapterStatus::Stubbed)
            .await
            .contains("not connected")
    );

    h.send(AdapterCommand::Connect {
        protocol: ProtocolId::Slack,
    });
    assert!(
        h.status(AdapterStatus::Connecting)
            .await
            .contains("browser")
    );
    let url = h.browser.last().expect("install page opened");
    assert!(url.starts_with("https://slack.com/oauth/v2/authorize?"));
    assert!(url.contains(CLIENT_ID));
    assert!(!url.contains(CLIENT_SECRET));
    assert!(!url.contains("user_scope"));
    let state = query_value(&url, "state");
    assert_eq!(
        h.vault.get_secret(SlackSecretKey::OAuthState).as_deref(),
        Some(state.as_str())
    );

    // A second Connect while waiting must not open a second install.
    h.send(AdapterCommand::Connect {
        protocol: ProtocolId::Slack,
    });
    h.status(AdapterStatus::Connecting).await;
    assert_eq!(h.browser.opened.lock().expect("browser").len(), 1);

    let reply = get(
        h.callback,
        &format!("/slack/oauth/callback?code={CODE}&state={state}"),
    )
    .await;
    assert!(reply.contains("200"));

    let ready = h.status(AdapterStatus::Ready).await;
    assert!(ready.contains("Fake Co"));
    assert!(ready.contains("Live updates are off"));
    let general = h.conversation("slack:C1").await;
    assert_eq!(general.title, "#general");
    assert_eq!(general.participant, "channel");

    let exchanged = h.api.with(|state| state.exchanged.clone());
    assert_eq!(exchanged.len(), 1);
    assert_eq!(exchanged[0].client_id, CLIENT_ID);
    assert_eq!(exchanged[0].client_secret, CLIENT_SECRET);
    assert_eq!(exchanged[0].code, CODE);
    assert_eq!(
        h.vault.get_secret(SlackSecretKey::BotToken).as_deref(),
        Some(BOT_TOKEN)
    );
    assert_eq!(
        h.vault.get_secret(SlackSecretKey::TeamId).as_deref(),
        Some("T1")
    );
    assert_eq!(h.vault.get_secret(SlackSecretKey::OAuthCode), None);
    assert_eq!(h.vault.get_secret(SlackSecretKey::OAuthState), None);
    assert!(!h.socket.connected());
}

#[tokio::test]
async fn channel_list_pages_skip_non_member_channels_and_name_dms() {
    let mut h = Harness::new(FakeApi::workspace(), installed_vault(), true);
    h.start();
    h.status(AdapterStatus::Ready).await;
    h.conversation("slack:C1").await;

    h.send(AdapterCommand::LoadChats {
        protocol: ProtocolId::Slack,
    });
    let dm = h.conversation("slack:D1").await;
    assert_eq!(dm.title, "Ana");
    assert_eq!(dm.participant, "direct message");
    let group = h.conversation("slack:G1").await;
    assert_eq!(group.title, "ana, bo");

    // The last page was loaded; another LoadChats does not call Slack again.
    h.send(AdapterCommand::LoadChats {
        protocol: ProtocolId::Slack,
    });
    h.send(AdapterCommand::Connect {
        protocol: ProtocolId::Slack,
    });
    h.status(AdapterStatus::Ready).await;
    let calls = h.calls();
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.starts_with("conversations.list"))
            .collect::<Vec<_>>(),
        ["conversations.list -", "conversations.list page-2"]
    );
    assert!(
        h.seen
            .iter()
            .all(|event| !matches!(event, AdapterEvent::ConversationUpsert { conversation } if conversation.id == "slack:C2")),
        "non-member channel must not be listed"
    );
    assert!(
        h.browser.last().is_none(),
        "a stored token must not reinstall"
    );
}

#[tokio::test]
async fn history_is_oldest_first_with_names_and_the_app_as_outbound() {
    let mut h = Harness::new(FakeApi::workspace(), installed_vault(), true);
    h.start();
    h.status(AdapterStatus::Ready).await;

    h.send(AdapterCommand::OpenChat {
        protocol: ProtocolId::Slack,
        conversation_id: "slack:C1".into(),
    });
    let first = h.message("first").await;
    assert_eq!(first.sender, "Ana");
    assert!(!first.outbound);
    assert_eq!(first.id, "slack:C1:1700000001.000100");
    let second = h.message("second, from the app").await;
    assert!(second.outbound);
    assert_eq!(second.conversation_id, "slack:C1");
}

#[tokio::test]
async fn send_posts_as_the_app_and_shows_the_sent_message() {
    let mut h = Harness::new(FakeApi::workspace(), installed_vault(), true);
    h.start();
    h.status(AdapterStatus::Ready).await;

    h.send(AdapterCommand::SendText {
        protocol: ProtocolId::Slack,
        conversation_id: "slack:C1".into(),
        body: "hello from thinwire".into(),
    });
    let sent = h.message("hello from thinwire").await;
    assert!(sent.outbound);
    assert_eq!(sent.sender, "thinwire");
    assert_eq!(
        h.api.with(|state| state.posted.clone()),
        [("C1".to_string(), "hello from thinwire".to_string())]
    );
}

#[tokio::test]
async fn socket_mode_messages_update_preview_and_unread_and_stop_on_disconnect() {
    let vault = installed_vault();
    vault.set_secret(SlackSecretKey::AppToken, APP_TOKEN);
    let mut h = Harness::new(FakeApi::workspace(), vault, true);
    h.start();
    let ready = h.status(AdapterStatus::Ready).await;
    assert!(!ready.contains("Live updates are off"));
    h.conversation("slack:C1").await;
    assert!(h.socket.connected());

    h.socket.push(SlackInbound::Message(post(
        "C1",
        "1700000200.000100",
        "U1",
        "live hello",
    )));
    let live = h.message("live hello").await;
    assert_eq!(live.sender, "Ana");
    let row = h
        .until("updated row", |event| {
            matches!(event, AdapterEvent::ConversationUpsert { conversation } if conversation.preview == "live hello")
        })
        .await;
    let AdapterEvent::ConversationUpsert { conversation } = row else {
        unreachable!();
    };
    assert_eq!(conversation.unread, 1);
    assert_eq!(conversation.order, 1_700_000_200);

    h.send(AdapterCommand::OpenChat {
        protocol: ProtocolId::Slack,
        conversation_id: "slack:C1".into(),
    });
    let read = h
        .until("unread reset", |event| {
            matches!(event, AdapterEvent::ConversationUpsert { conversation } if conversation.id == "slack:C1" && conversation.unread == 0)
        })
        .await;
    assert!(matches!(read, AdapterEvent::ConversationUpsert { .. }));

    h.send(AdapterCommand::Disconnect {
        protocol: ProtocolId::Slack,
    });
    assert!(
        h.status(AdapterStatus::Stubbed)
            .await
            .contains("disconnected")
    );
    assert!(h.socket.stopped.load(Ordering::SeqCst));
    assert_eq!(
        h.vault.get_secret(SlackSecretKey::BotToken).as_deref(),
        Some(BOT_TOKEN),
        "disconnect keeps the install"
    );
}

#[tokio::test]
async fn revoked_token_clears_the_install() {
    let api = FakeApi::workspace();
    api.with(|state| state.identify_error = Some(SlackApiError::api("token_revoked")));
    let mut h = Harness::new(api, installed_vault(), true);
    h.start();
    let detail = h.status(AdapterStatus::Error).await;
    assert!(detail.contains("access ended"));
    assert_eq!(h.vault.get_secret(SlackSecretKey::BotToken), None);
    assert_eq!(h.vault.get_secret(SlackSecretKey::TeamId), None);
}

#[tokio::test]
async fn app_uninstall_event_clears_the_install() {
    let vault = installed_vault();
    vault.set_secret(SlackSecretKey::AppToken, APP_TOKEN);
    let mut h = Harness::new(FakeApi::workspace(), vault, true);
    h.start();
    h.status(AdapterStatus::Ready).await;
    h.socket.push(SlackInbound::Revoked);
    assert!(
        h.status(AdapterStatus::Error)
            .await
            .contains("access ended")
    );
    assert!(h.socket.stopped.load(Ordering::SeqCst));
    assert_eq!(h.vault.get_secret(SlackSecretKey::BotToken), None);
}

#[tokio::test]
async fn not_in_channel_asks_to_add_the_app() {
    let api = FakeApi::workspace();
    api.with(|state| state.history_error = Some(SlackApiError::api("not_in_channel")));
    let mut h = Harness::new(api, installed_vault(), true);
    h.start();
    h.status(AdapterStatus::Ready).await;
    h.send(AdapterCommand::OpenChat {
        protocol: ProtocolId::Slack,
        conversation_id: "slack:C1".into(),
    });
    assert!(h.status(AdapterStatus::Error).await.contains("Add the app"));
    assert_eq!(
        h.vault.get_secret(SlackSecretKey::BotToken).as_deref(),
        Some(BOT_TOKEN)
    );
}

#[tokio::test]
async fn declined_install_stores_nothing() {
    let mut h = Harness::new(FakeApi::workspace(), client_vault(), true);
    h.start();
    h.send(AdapterCommand::Connect {
        protocol: ProtocolId::Slack,
    });
    h.status(AdapterStatus::Connecting).await;
    let state = query_value(&h.browser.last().expect("opened"), "state");
    get(
        h.callback,
        &format!("/slack/oauth/callback?error=access_denied&state={state}"),
    )
    .await;
    assert!(h.status(AdapterStatus::Error).await.contains("cancelled"));
    assert_eq!(h.vault.get_secret(SlackSecretKey::BotToken), None);
    assert_eq!(h.vault.get_secret(SlackSecretKey::OAuthState), None);
    assert!(h.api.with(|state| state.exchanged.is_empty()));
}

#[tokio::test]
async fn disconnect_cancels_a_pending_install() {
    let mut h = Harness::new(FakeApi::workspace(), client_vault(), true);
    h.start();
    h.send(AdapterCommand::Connect {
        protocol: ProtocolId::Slack,
    });
    h.status(AdapterStatus::Connecting).await;
    h.send(AdapterCommand::Disconnect {
        protocol: ProtocolId::Slack,
    });
    h.status(AdapterStatus::Stubbed).await;
    assert_eq!(h.vault.get_secret(SlackSecretKey::OAuthState), None);
    // The listener is gone, so a new install can bind the port again.
    h.send(AdapterCommand::Connect {
        protocol: ProtocolId::Slack,
    });
    h.status(AdapterStatus::Connecting).await;
    assert_eq!(h.browser.opened.lock().expect("browser").len(), 2);
}

#[tokio::test]
async fn missing_client_credentials_or_browser_fail_without_waiting() {
    let mut h = Harness::new(FakeApi::workspace(), MemorySlackVault::new(), true);
    h.start();
    h.send(AdapterCommand::Connect {
        protocol: ProtocolId::Slack,
    });
    assert!(
        h.status(AdapterStatus::Error)
            .await
            .contains("credentials are missing")
    );
    assert!(h.browser.last().is_none());

    let mut h = Harness::new(FakeApi::workspace(), client_vault(), false);
    h.start();
    h.send(AdapterCommand::Connect {
        protocol: ProtocolId::Slack,
    });
    assert!(h.status(AdapterStatus::Error).await.contains("browser"));
    assert_eq!(h.vault.get_secret(SlackSecretKey::OAuthState), None);
}
