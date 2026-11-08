//! Live `tdlib-rs` client. Compiled only with `--features telegram-tdlib`.
//!
//! FFI and network I/O stay on a dedicated receive thread plus tokio tasks.
//! Credential values are read from the vault and never logged.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use super::credentials::{TelegramApiSource, parse_resolved_api_id, require_resolved_api};
use super::data_dir;
use super::inbox::{
    self, ChatDirectory, ChatEffect, ChatMute, ChatSeed, InboxMessage, MessageParty, MuteScope,
    NameBook, OlderHistory, OlderStep, PageOutcome,
};
use super::lifecycle::{
    CloseKind, ClosingFlag, DoneFlag, LateReady, LoginEvents, WorkerSlots, all_done, close_kind,
    late_ready, mark_done, wait_until,
};
use super::router::{self, Router};
use crate::adapter::{
    AdapterEvent, AdapterStatus, Arrival, ChatMessage, ChatTopic, Delivery, EventTx, LoginEpoch,
    MessageTopic, OutgoingUpdate, ProtocolId, TelegramAuthError, TelegramAuthPhase,
    TelegramAuthStep, TelegramCodeVia, emit_chat_list_loaded, emit_conversation,
    emit_conversation_removed, emit_history_loaded, emit_message, emit_message_delivery,
    emit_message_edited, emit_message_links, emit_message_media, emit_message_replaced,
    emit_messages_removed, emit_older_history_loaded, emit_outbox_read, emit_photo_failed,
    emit_photo_ready, emit_send_accepted, emit_send_rejected, emit_status, emit_stopped,
    emit_telegram_session_ended, emit_topic, outgoing_delivery,
};
use crate::secrets::{TelegramSecretKey, TelegramSecretVault};

/// Marker written to the persistent session key after authorization.
pub const TDLIB_SESSION_MARKER: &str = "tdlib-ready";

#[derive(Clone)]
pub(super) enum TdlibCommand {
    Step(TelegramAuthStep),
    LoadChats,
    OpenChat(String),
    SendText {
        conversation_id: String,
        body: String,
        request: u64,
        reply_to: Option<String>,
    },
    /// Delete one message. `for_everyone` is TDLib `revoke`.
    DeleteMessage {
        conversation_id: String,
        message_id: String,
        for_everyone: bool,
    },
    /// Add or remove the emoji reaction of this account.
    React {
        conversation_id: String,
        message_id: String,
        emoji: String,
        add: bool,
    },
    /// Put `body` in the place of the text of one own message.
    EditMessage {
        conversation_id: String,
        message_id: String,
        body: String,
    },
    /// Forward messages of one chat, with their media, to another chat.
    ForwardMessages {
        from_conversation_id: String,
        message_ids: Vec<String>,
        to_conversation_id: String,
    },
    /// Tell the chat, or one topic of it, that this account types or stopped.
    SetTyping {
        conversation_id: String,
        topic: Option<MessageTopic>,
        typing: bool,
    },
    Resend {
        conversation_id: String,
        message_id: String,
        request: u64,
    },
    LoadOlder {
        conversation_id: String,
        before_message_id: String,
    },
    OpenTopic {
        conversation_id: String,
        topic: MessageTopic,
    },
    LoadOlderTopic {
        conversation_id: String,
        topic: MessageTopic,
        before_message_id: String,
    },
    SendTopicText {
        conversation_id: String,
        topic: MessageTopic,
        body: String,
        request: u64,
        reply_to: Option<String>,
    },
    /// The chat the user looks at. `None` means they left Telegram.
    /// This, not history load, owns `LiveInbox::open_chat`.
    /// `stamp` is the shared-cell write from the caller. The worker applies
    /// it only while that write is still current, so a blur during
    /// `getChatHistory` is not overwritten when this command is dequeued.
    ViewChat {
        conversation_id: Option<String>,
        stamp: u64,
    },
    /// The topic the user looks at. History of another topic stays unread.
    ViewTopic {
        conversation_id: String,
        topic: MessageTopic,
        stamp: u64,
    },
    /// Ask TDLib to close. The worker exits after `authorizationStateClosed`.
    /// `cancel` is true for a Cancel (Disconnect): a client that already became
    /// Ready is then rolled back (see [`close_kind`]).
    Close {
        cancel: bool,
    },
    /// Download one photo. `secret` deletes the file after the bytes are read.
    DownloadFile {
        file_id: i32,
        secret: bool,
    },
    /// Download one voice file. The path stays. The bytes are not read.
    DownloadVoice {
        file_id: i32,
    },
    /// Download one video file for the in-app player. The path stays.
    DownloadVideo {
        file_id: i32,
    },
    /// Download one document. The path stays. The bytes are not read.
    DownloadDocument {
        file_id: i32,
    },
    /// Send photos and videos, with the caption on the first file.
    SendMedia {
        conversation_id: String,
        topic: Option<MessageTopic>,
        files: Vec<crate::media::OutgoingFile>,
        caption: String,
        request: u64,
    },
}

impl TdlibCommand {
    /// The name of the command for a log line. It has no user data.
    const fn kind(&self) -> &'static str {
        match self {
            Self::Step(_) => "step",
            Self::LoadChats => "load_chats",
            Self::OpenChat(_) => "open_chat",
            Self::SendText { .. } => "send_text",
            Self::DeleteMessage { .. } => "delete_message",
            Self::React { .. } => "react",
            Self::EditMessage { .. } => "edit_message",
            Self::ForwardMessages { .. } => "forward_messages",
            Self::SetTyping { .. } => "set_typing",
            Self::Resend { .. } => "resend",
            Self::LoadOlder { .. } => "load_older",
            Self::OpenTopic { .. } => "open_topic",
            Self::LoadOlderTopic { .. } => "load_older_topic",
            Self::SendTopicText { .. } => "send_topic_text",
            Self::ViewChat { .. } => "view_chat",
            Self::ViewTopic { .. } => "view_topic",
            Self::Close { .. } => "close",
            Self::DownloadFile { .. } => "download_file",
            Self::DownloadVoice { .. } => "download_voice",
            Self::DownloadVideo { .. } => "download_video",
            Self::DownloadDocument { .. } => "download_document",
            Self::SendMedia { .. } => "send_media",
        }
    }
}

/// How often a waiter checks that closing workers have exited.
const WORKER_POLL: Duration = Duration::from_millis(50);

/// Longest wait for an old client to release the database before a new one
/// opens it.
const RETIRE_TIMEOUT: Duration = Duration::from_secs(10);

/// Longest wait in `shutdown`. It is below the app's 5 s close limit, so
/// `Stopped` always reaches the UI before the window gives up.
const SHUTDOWN_LIMIT: Duration = Duration::from_secs(4);

struct LiveInbox {
    authorized: bool,
    older: OlderHistory,
    /// This worker asked for a phone, code, or password step. Only such a
    /// login is rolled back on Cancel; a resumed session never is (ux F8).
    new_login: bool,
    /// TDLib became Ready, but the worker skipped it (Cancel or shutdown).
    /// `close_kind` treats it as signed in (PR #49 review).
    late_ready: bool,
    /// `cancel` of the `Close` the worker read. `None` before that.
    close_cancel: Option<bool>,
    /// This app asked TDLib to close (shutdown, Cancel, Try again). The runtime
    /// sets it before the worker reads `Close`. A close without it came from
    /// elsewhere, for example a remote logout.
    closing: ClosingFlag,
    /// The worker already sent TDLib `close`.
    close_requested: bool,
    /// A live session started to close without our request. Reported once
    /// the client is fully closed (qa R74).
    ended_elsewhere: bool,
    directory: ChatDirectory,
    names: NameBook,
    /// Chat `ViewChat` last named. A live message there is marked viewed.
    /// History load does not set this: the user can leave that thread.
    open_chat: Option<i64>,
    /// The topic `ViewTopic` last named. `None` is the topic list or a normal chat.
    open_topic: Option<MessageTopic>,
    /// Forum topics already loaded, so a later update can keep the name,
    /// the unread count, and the last message. This map lives only as long
    /// as the worker: an account end drops it with [`LiveInbox`]. A chat
    /// that leaves the list drops its rows here too, because the core drops
    /// them with the conversation.
    topics: HashMap<(i64, i32), ChatTopic>,
    /// Main-list forums whose topic list loaded, and the wait of each failed
    /// list before the next try.
    topic_lists: inbox::TopicLists,
    /// Same fact, published by `view_chat` before this worker dequeues
    /// `ViewChat`. History `viewMessages` reads this, not `open_chat`.
    viewed: inbox::ViewedChat,
    /// Newest message id per chat that was not passed to `viewMessages`:
    /// a live message while the chat was not viewed, or a history page that
    /// returned after the window blurred. Focus calls `viewMessages` through
    /// this id. The id stays until that call succeeds (#174 items 12 and 13).
    unviewed: Arc<Mutex<HashMap<i64, i64>>>,
    /// Downloads this worker started.
    downloads: HashMap<i32, InFlight>,
}

#[derive(Clone, Copy)]
enum InFlight {
    Photo { secret: bool, saw_active: bool },
    Voice { saw_active: bool },
    Video { saw_active: bool },
    File { saw_active: bool },
}

impl InFlight {
    fn saw_active(self) -> bool {
        match self {
            Self::Photo { saw_active, .. }
            | Self::Voice { saw_active }
            | Self::Video { saw_active }
            | Self::File { saw_active } => saw_active,
        }
    }

    fn mark_active(&mut self) {
        match self {
            Self::Photo { saw_active, .. }
            | Self::Voice { saw_active }
            | Self::Video { saw_active }
            | Self::File { saw_active } => *saw_active = true,
        }
    }
}

impl LiveInbox {
    /// Signed in, not closing, and the login epoch is still current. Every
    /// inbox event checks this, so Cancel stops the chat list and messages
    /// at the worker (PR #49 review).
    fn linked(&self) -> bool {
        self.authorized && self.closing.current()
    }

    /// Apply one chat-list change and emit it when `emit` is true.
    fn publish(&mut self, events: &EventTx, emit: bool, effect: Option<ChatEffect>) {
        // A chat that leaves the list drops its paging state too, even before
        // Ready (PR #52 review). Its cached topics go with it: the core drops
        // those rows, and the next message has to fetch them again.
        self.older.follow(effect.as_ref());
        if let Some(ChatEffect::Remove(id)) = effect.as_ref()
            && let Some(chat_id) = inbox::parse_telegram_chat_id(id)
        {
            inbox::forget_chat_topics(&mut self.topics, &mut self.topic_lists, chat_id);
        }
        if emit {
            emit_effect(events, effect);
        }
    }
}

/// Owns the command sink into the current worker and the workers' done flags.
///
/// A worker never drops its TDLib client without `close`: an unclean exit
/// aborts the process at exit and can damage the TDLib database.
pub struct TdlibRuntime {
    commands: Option<UnboundedSender<TdlibCommand>>,
    slots: WorkerSlots,
    /// Closing flag of the current worker. `stop()` marks it at once.
    current_closing: Option<ClosingFlag>,
    /// The host's Telegram login epoch. A new worker stamps its login events
    /// with the value it starts with.
    login_epoch: LoginEpoch,
    /// Viewed chat of the current worker. `view_chat` writes it at once.
    /// A respawn replaces it, so a closing worker cannot clear the new one.
    viewed: inbox::ViewedChat,
    /// Last `ViewChat`, including one that arrived while no worker was running.
    /// Ready copies it into the new cell. A restart does not drop it, and
    /// storing it does not start a client.
    requested_view: Arc<Mutex<Option<String>>>,
}

impl TdlibRuntime {
    #[must_use]
    pub fn new() -> Self {
        Self::with_login_epoch(Arc::default())
    }

    #[must_use]
    pub(crate) fn with_login_epoch(login_epoch: LoginEpoch) -> Self {
        Self {
            login_epoch,
            commands: None,
            slots: WorkerSlots::new(),
            current_closing: None,
            viewed: inbox::ViewedChat::new(),
            requested_view: Arc::new(Mutex::new(None)),
        }
    }

    /// Close the current worker's TDLib client. The next command starts a new
    /// worker, which waits until this one released the database.
    pub fn stop(&mut self) {
        self.stop_client(true);
    }

    /// `cancel`: a Cancel rolls back a client that already became Ready; app
    /// shutdown keeps the session.
    fn stop_client(&mut self, cancel: bool) {
        // Mark first: from now on the old client sends no login events, even
        // before its worker reads `Close` (issue #42). The viewed chat goes
        // with it, so a message that arrives while closing is not marked read.
        if let Some(closing) = self.current_closing.take() {
            closing.mark();
        }
        self.viewed.clear();
        if let Some(commands) = self.commands.take() {
            let _ = commands.send(TdlibCommand::Close { cancel });
        }
        self.slots.retire_current();
    }

    /// Close every TDLib client, then emit `Stopped` once all workers exited.
    pub fn shutdown(&mut self, events: &EventTx) {
        self.stop_client(false);
        self.slots.shut_down();
        let workers = self.slots.all();
        let events = events.clone();
        tokio::spawn(async move {
            // Exit only when no thread is inside TDLib any more, or at the limit.
            let clean = wait_until(
                || all_done(&workers) && receiver_idle(),
                SHUTDOWN_LIMIT,
                WORKER_POLL,
            )
            .await;
            if clean {
                data_dir::remove_this_process_session_dir();
            } else {
                tracing::warn!("telegram did not close within the shutdown limit");
            }
            emit_stopped(&events, ProtocolId::Telegram);
        });
    }

    pub fn submit(
        &mut self,
        step: TelegramAuthStep,
        secrets: Arc<dyn TelegramSecretVault>,
        source: TelegramApiSource,
        events: &EventTx,
    ) {
        self.enqueue(TdlibCommand::Step(step), secrets, source, events);
    }

    pub fn load_chats(
        &mut self,
        secrets: Arc<dyn TelegramSecretVault>,
        source: TelegramApiSource,
        events: &EventTx,
    ) {
        self.enqueue(TdlibCommand::LoadChats, secrets, source, events);
    }

    pub fn open_chat(
        &mut self,
        conversation_id: String,
        secrets: Arc<dyn TelegramSecretVault>,
        source: TelegramApiSource,
        events: &EventTx,
    ) {
        self.enqueue(
            TdlibCommand::OpenChat(conversation_id),
            secrets,
            source,
            events,
        );
    }

    /// The chat the user looks at. `None` means they left.
    /// The request is kept when no worker is running, and that must not start one.
    /// Ready copies it into the new worker. The shared cell updates before the
    /// send, so a history load still inside `getChatHistory` sees the new view
    /// before this command is dequeued.
    pub fn view_chat(&mut self, conversation_id: Option<String>) {
        let stamp = {
            let mut requested = self
                .requested_view
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            *requested = conversation_id.clone();
            // No worker: keep the request for Ready. Do not start a client.
            if self.commands.is_none() {
                return;
            }
            // Publish before releasing the request lock. Ready reads and
            // publishes under the same lock, so it cannot restore a chat
            // this write already replaced.
            self.viewed
                .publish(inbox::viewed_chat_id(conversation_id.as_deref()))
        };
        if let Some(commands) = &self.commands {
            let _ = commands.send(TdlibCommand::ViewChat {
                conversation_id,
                stamp,
            });
        }
    }

    /// The topic the user looks at. The shared cell updates before the send,
    /// so a topic history load sees this topic before the command is dequeued.
    pub fn view_topic(&mut self, conversation_id: String, topic: MessageTopic) {
        let stamp = {
            let mut requested = self
                .requested_view
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            *requested = Some(conversation_id.clone());
            if self.commands.is_none() {
                return;
            }
            self.viewed
                .publish_topic(inbox::viewed_chat_id(Some(&conversation_id)), Some(topic))
        };
        if let Some(commands) = &self.commands {
            let _ = commands.send(TdlibCommand::ViewTopic {
                conversation_id,
                topic,
                stamp,
            });
        }
    }

    pub fn open_topic(
        &mut self,
        conversation_id: String,
        topic: MessageTopic,
        secrets: Arc<dyn TelegramSecretVault>,
        source: TelegramApiSource,
        events: &EventTx,
    ) {
        self.enqueue(
            TdlibCommand::OpenTopic {
                conversation_id,
                topic,
            },
            secrets,
            source,
            events,
        );
    }

    pub fn load_older_topic(
        &mut self,
        conversation_id: String,
        topic: MessageTopic,
        before_message_id: String,
        secrets: Arc<dyn TelegramSecretVault>,
        source: TelegramApiSource,
        events: &EventTx,
    ) {
        self.enqueue(
            TdlibCommand::LoadOlderTopic {
                conversation_id,
                topic,
                before_message_id,
            },
            secrets,
            source,
            events,
        );
    }

    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    pub fn send_topic_text(
        &mut self,
        conversation_id: String,
        topic: MessageTopic,
        body: String,
        request: u64,
        reply_to: Option<String>,
        secrets: Arc<dyn TelegramSecretVault>,
        source: TelegramApiSource,
        events: &EventTx,
    ) {
        self.enqueue(
            TdlibCommand::SendTopicText {
                conversation_id,
                topic,
                body,
                request,
                reply_to,
            },
            secrets,
            source,
            events,
        );
    }

    pub fn download_file(
        &mut self,
        file_id: i32,
        secret: bool,
        secrets: Arc<dyn TelegramSecretVault>,
        source: TelegramApiSource,
        events: &EventTx,
    ) {
        self.enqueue(
            TdlibCommand::DownloadFile { file_id, secret },
            secrets,
            source,
            events,
        );
    }

    pub fn download_voice(
        &mut self,
        file_id: i32,
        secrets: Arc<dyn TelegramSecretVault>,
        source: TelegramApiSource,
        events: &EventTx,
    ) {
        self.enqueue(
            TdlibCommand::DownloadVoice { file_id },
            secrets,
            source,
            events,
        );
    }

    pub fn download_document(
        &mut self,
        file_id: i32,
        secrets: Arc<dyn TelegramSecretVault>,
        source: TelegramApiSource,
        events: &EventTx,
    ) {
        self.enqueue(
            TdlibCommand::DownloadDocument { file_id },
            secrets,
            source,
            events,
        );
    }

    pub fn download_video(
        &mut self,
        file_id: i32,
        secrets: Arc<dyn TelegramSecretVault>,
        source: TelegramApiSource,
        events: &EventTx,
    ) {
        self.enqueue(
            TdlibCommand::DownloadVideo { file_id },
            secrets,
            source,
            events,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn send_media(
        &mut self,
        conversation_id: String,
        topic: Option<MessageTopic>,
        files: Vec<crate::media::OutgoingFile>,
        caption: String,
        request: u64,
        secrets: Arc<dyn TelegramSecretVault>,
        source: TelegramApiSource,
        events: &EventTx,
    ) {
        self.enqueue(
            TdlibCommand::SendMedia {
                conversation_id,
                topic,
                files,
                caption,
                request,
            },
            secrets,
            source,
            events,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn send_text(
        &mut self,
        conversation_id: String,
        body: String,
        request: u64,
        reply_to: Option<String>,
        secrets: Arc<dyn TelegramSecretVault>,
        source: TelegramApiSource,
        events: &EventTx,
    ) {
        self.enqueue(
            TdlibCommand::SendText {
                conversation_id,
                body,
                request,
                reply_to,
            },
            secrets,
            source,
            events,
        );
    }

    /// Queue a delete or a reaction of the message menu.
    pub fn enqueue_message_action(
        &mut self,
        command: TdlibCommand,
        secrets: Arc<dyn TelegramSecretVault>,
        source: TelegramApiSource,
        events: &EventTx,
    ) {
        self.enqueue(command, secrets, source, events);
    }

    pub fn load_older(
        &mut self,
        conversation_id: String,
        before_message_id: String,
        secrets: Arc<dyn TelegramSecretVault>,
        source: TelegramApiSource,
        events: &EventTx,
    ) {
        self.enqueue(
            TdlibCommand::LoadOlder {
                conversation_id,
                before_message_id,
            },
            secrets,
            source,
            events,
        );
    }

    pub fn resend(
        &mut self,
        conversation_id: String,
        message_id: String,
        request: u64,
        secrets: Arc<dyn TelegramSecretVault>,
        source: TelegramApiSource,
        events: &EventTx,
    ) {
        self.enqueue(
            TdlibCommand::Resend {
                conversation_id,
                message_id,
                request,
            },
            secrets,
            source,
            events,
        );
    }

    fn enqueue(
        &mut self,
        command: TdlibCommand,
        secrets: Arc<dyn TelegramSecretVault>,
        source: TelegramApiSource,
        events: &EventTx,
    ) {
        // The app is closing: a late UI command must not open a new client.
        if self.slots.is_shut() {
            return;
        }
        let TdlibRuntime {
            commands,
            slots,
            current_closing,
            login_epoch,
            viewed,
            requested_view,
        } = self;
        let login_epoch = Arc::clone(login_epoch);
        let requested_view = Arc::clone(requested_view);
        let sent = super::send_or_respawn(commands, command, || {
            let (done, wait_for) = slots.start().unwrap_or_default();
            let closing = ClosingFlag::new(Arc::clone(&login_epoch));
            *current_closing = Some(closing.clone());
            // A new client gets a new cell. The previous worker clears its
            // own on close and must not wipe a view this client publishes.
            let next = inbox::ViewedChat::new();
            *viewed = next.clone();
            spawn_tdlib_worker(
                Arc::clone(&secrets),
                source.clone(),
                events.clone(),
                done,
                wait_for,
                closing,
                WorkerView {
                    viewed: next,
                    requested: Arc::clone(&requested_view),
                },
            )
        });
        if !sent {
            emit_status(
                events,
                ProtocolId::Telegram,
                AdapterStatus::Error,
                "Telegram stopped. Cancel and try again.",
            );
        }
    }
}

impl Default for TdlibRuntime {
    fn default() -> Self {
        Self::new()
    }
}

/// The one process-wide receive thread and its router.
struct Dispatcher {
    router: Mutex<Router<tdlib_rs::enums::Update>>,
    wake: Condvar,
    /// The receive thread started. If it did not, nothing is inside TDLib.
    running: AtomicBool,
}

static DISPATCHER: OnceLock<Arc<Dispatcher>> = OnceLock::new();

impl Dispatcher {
    fn get() -> Arc<Self> {
        Arc::clone(DISPATCHER.get_or_init(|| {
            let dispatcher = Arc::new(Self {
                router: Mutex::new(Router::new()),
                wake: Condvar::new(),
                running: AtomicBool::new(false),
            });
            let receiving = Arc::clone(&dispatcher);
            let spawned = thread::Builder::new()
                .name("thinwire-tdlib-recv".into())
                .spawn(move || receiving.receive_loop());
            match spawned {
                Ok(_) => dispatcher.running.store(true, Ordering::SeqCst),
                Err(error) => tracing::warn!(%error, "tdlib receive thread did not start"),
            }
            dispatcher
        }))
    }

    fn lock(&self) -> MutexGuard<'_, Router<tdlib_rs::enums::Update>> {
        self.router.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Call `td_receive` only while a client exists. `receive` returns None on
    /// timeout and after it hands a response to the request observer.
    fn receive_loop(&self) {
        loop {
            {
                let mut router = self.lock();
                while !router.begin_receive() {
                    router = self
                        .wake
                        .wait(router)
                        .unwrap_or_else(PoisonError::into_inner);
                }
            }
            let received = tdlib_rs::receive();
            let mut router = self.lock();
            router.end_receive();
            if let Some((update, client_id)) = received {
                router.route(client_id, update);
            }
        }
    }

    fn register(&self, client_id: i32) -> UnboundedReceiver<tdlib_rs::enums::Update> {
        let receiver = self.lock().register(client_id);
        self.wake.notify_all();
        receiver
    }

    fn unregister(&self, client_id: i32) {
        self.lock().unregister(client_id);
    }
}

/// True when no thread is inside TDLib: no client and no receive in flight.
fn receiver_idle() -> bool {
    DISPATCHER.get().is_none_or(|dispatcher| {
        router::receiver_idle(
            dispatcher.running.load(Ordering::SeqCst),
            &dispatcher.lock(),
        )
    })
}

struct WorkerView {
    viewed: inbox::ViewedChat,
    requested: Arc<Mutex<Option<String>>>,
}

fn spawn_tdlib_worker(
    secrets: Arc<dyn TelegramSecretVault>,
    source: TelegramApiSource,
    events: EventTx,
    done: DoneFlag,
    wait_for: Vec<DoneFlag>,
    closing: ClosingFlag,
    view: WorkerView,
) -> UnboundedSender<TdlibCommand> {
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();

    tokio::spawn(async move {
        // Commands wait in the channel until the old client released the database.
        if !wait_for_retired(&wait_for).await {
            emit_status(
                &events,
                ProtocolId::Telegram,
                AdapterStatus::Error,
                "Telegram is still closing the last session. Cancel and try again.",
            );
            mark_done(&done);
            return;
        }
        let dispatcher = Dispatcher::get();
        let client_id = tdlib_rs::create_client();
        // Register before the first request, so no update for this client is lost.
        let mut update_rx = dispatcher.register(client_id);

        if tdlib_rs::functions::set_log_verbosity_level(TDLIB_LOG_LEVEL, client_id)
            .await
            .is_err()
        {
            tracing::info!("tdlib log verbosity was not applied");
        }

        let mut live = LiveInbox {
            authorized: false,
            older: OlderHistory::new(),
            new_login: false,
            closing: closing.clone(),
            close_requested: false,
            late_ready: false,
            close_cancel: None,
            ended_elsewhere: false,
            directory: ChatDirectory::new(),
            names: NameBook::new(),
            open_chat: None,
            open_topic: None,
            topics: HashMap::new(),
            topic_lists: inbox::TopicLists::default(),
            viewed: view.viewed,
            unviewed: Arc::new(Mutex::new(HashMap::new())),
            downloads: HashMap::new(),
        };
        let requested_view = view.requested;
        let mut commands = Some(cmd_rx);

        loop {
            tokio::select! {
                command = next_command(&mut commands) => {
                    // A dropped sender means the runtime let go of this worker.
                    // Close TDLib in both cases; keep reading updates until Closed.
                    let command = command.unwrap_or_else(|| {
                        commands = None;
                        TdlibCommand::Close { cancel: false }
                    });
                    if live.closing.is_set() && !matches!(command, TdlibCommand::Close { .. }) {
                        continue;
                    }
                    let kind = command.kind();
                    let started = Instant::now();
                    match command {
                        TdlibCommand::Close { cancel } => {
                            if live.close_requested {
                                continue;
                            }
                            live.close_requested = true;
                            live.close_cancel = Some(cancel);
                            live.closing.mark();
                            live.viewed.clear();
                            live.open_chat = None;
                            live.open_topic = None;
                            let signed_in = live.authorized || live.late_ready;
                            match close_kind(signed_in, live.new_login, cancel) {
                                CloseKind::LogOut => {
                                    // Ready ran before Cancel, but the UI dropped
                                    // it: do not keep that session (PR #49 review).
                                    secrets.set_secret(TelegramSecretKey::Session, "");
                                    super::request_secret_flush(&events);
                                    request_log_out(client_id).await;
                                }
                                CloseKind::Close => request_close(client_id).await,
                            }
                        }
                        TdlibCommand::Step(step) => {
                            apply_step(client_id, step, secrets.as_ref(), &events, &live.closing).await;
                        }
                        TdlibCommand::LoadChats => {
                            load_main_chats(client_id, live.linked(), &events).await;
                            load_due_forum_topics(client_id, &mut live, &events).await;
                        }
                        TdlibCommand::OpenChat(conversation_id) => {
                            open_chat(client_id, &conversation_id, &mut live, &events).await;
                        }
                        TdlibCommand::OpenTopic {
                            conversation_id,
                            topic,
                        } => {
                            open_topic(client_id, &conversation_id, topic, &mut live, &events).await;
                        }
                        TdlibCommand::ViewChat {
                            conversation_id,
                            stamp,
                        } => {
                            apply_view_command(
                                &live.viewed,
                                &mut live.open_chat,
                                &mut live.open_topic,
                                conversation_id.as_deref(),
                                stamp,
                            );
                            // Messages that arrived while this chat was not
                            // viewed, and a history page skipped after a blur,
                            // never reached viewMessages. Mark through the
                            // latest id now. `force_read` covers that id.
                            // The id stays until the call succeeds (#174 item 13).
                            // Focus can run again before that success, so
                            // viewMessages may be sent more than once. The
                            // call is idempotent.
                            let pending = {
                                let unviewed =
                                    live.unviewed.lock().unwrap_or_else(PoisonError::into_inner);
                                catch_up_read(live.open_chat, &unviewed)
                            };
                            if let Some((chat_id, message_id)) = pending {
                                let unviewed = Arc::clone(&live.unviewed);
                                tokio::spawn(async move {
                                    if mark_message_viewed(client_id, chat_id, message_id).await {
                                        let mut waiting = unviewed
                                            .lock()
                                            .unwrap_or_else(PoisonError::into_inner);
                                        note_view_succeeded(&mut waiting, chat_id, message_id);
                                    }
                                });
                            }
                        }
                        TdlibCommand::ViewTopic {
                            conversation_id,
                            topic,
                            stamp,
                        } => {
                            let chat = inbox::viewed_chat_id(Some(&conversation_id));
                            if live.viewed.sync_topic(stamp, chat, Some(topic)) {
                                live.open_chat = chat;
                                live.open_topic = Some(topic);
                            } else {
                                live.open_chat = live.viewed.get();
                                live.open_topic = live.viewed.get_topic();
                            }
                        }
                        TdlibCommand::SendMedia {
                            conversation_id,
                            topic,
                            files,
                            caption,
                            request,
                        } => {
                            send_media(
                                client_id,
                                &conversation_id,
                                topic,
                                &files,
                                &caption,
                                request,
                                &live,
                                &events,
                            )
                            .await;
                        }
                        TdlibCommand::SendText {
                            conversation_id,
                            body,
                            request,
                            reply_to,
                        } => {
                            send_text(
                                client_id,
                                &conversation_id,
                                &body,
                                request,
                                None,
                                reply_to.as_deref(),
                                &live,
                                &events,
                            )
                            .await;
                        }
                        TdlibCommand::DeleteMessage {
                            conversation_id,
                            message_id,
                            for_everyone,
                        } => {
                            delete_message(
                                client_id,
                                &conversation_id,
                                &message_id,
                                for_everyone,
                                &events,
                            )
                            .await;
                        }
                        TdlibCommand::EditMessage {
                            conversation_id,
                            message_id,
                            body,
                        } => {
                            edit_message(
                                client_id,
                                &conversation_id,
                                &message_id,
                                &body,
                                live.linked(),
                                &events,
                            )
                            .await;
                        }
                        TdlibCommand::ForwardMessages {
                            from_conversation_id,
                            message_ids,
                            to_conversation_id,
                        } => {
                            forward_messages(
                                client_id,
                                &from_conversation_id,
                                &message_ids,
                                &to_conversation_id,
                                &events,
                            )
                            .await;
                        }
                        TdlibCommand::SetTyping {
                            conversation_id,
                            topic,
                            typing,
                        } => {
                            // A round trip here would stall every later
                            // update. A client that is not linked sends none.
                            if live.linked()
                                && let Some((chat_id, topic)) =
                                    typing_target(&conversation_id, topic)
                            {
                                tokio::spawn(send_typing(client_id, chat_id, topic, typing));
                            }
                        }
                        TdlibCommand::React {
                            conversation_id,
                            message_id,
                            emoji,
                            add,
                        } => {
                            react(client_id, &conversation_id, &message_id, &emoji, add, &events)
                                .await;
                        }
                        TdlibCommand::SendTopicText {
                            conversation_id,
                            topic,
                            body,
                            request,
                            reply_to,
                        } => {
                            send_text(
                                client_id,
                                &conversation_id,
                                &body,
                                request,
                                Some(topic),
                                reply_to.as_deref(),
                                &live,
                                &events,
                            )
                            .await;
                        }
                        TdlibCommand::LoadOlder {
                            conversation_id,
                            before_message_id,
                        } => {
                            load_older(
                                client_id,
                                &conversation_id,
                                &before_message_id,
                                &mut live,
                                &events,
                            )
                            .await;
                        }
                        TdlibCommand::LoadOlderTopic {
                            conversation_id,
                            topic,
                            before_message_id,
                        } => {
                            load_older_topic(
                                client_id,
                                &conversation_id,
                                topic,
                                &before_message_id,
                                &mut live,
                                &events,
                            )
                            .await;
                        }
                        TdlibCommand::Resend { conversation_id, message_id, request } => {
                            resend(client_id, &conversation_id, &message_id, request, &live, &events).await;
                        }
                        TdlibCommand::DownloadFile { file_id, secret } => {
                            start_download(client_id, file_id, secret, &mut live, &events).await;
                        }
                        TdlibCommand::DownloadVoice { file_id } => {
                            start_voice(client_id, file_id, &mut live, &events).await;
                        }
                        TdlibCommand::DownloadVideo { file_id } => {
                            begin_download(
                                client_id,
                                file_id,
                                InFlight::Video { saw_active: false },
                                &mut live,
                                &events,
                            )
                            .await;
                        }
                        TdlibCommand::DownloadDocument { file_id } => {
                            begin_download(
                                client_id,
                                file_id,
                                InFlight::File { saw_active: false },
                                &mut live,
                                &events,
                            )
                            .await;
                        }
                    }
                    note_slow_step("command", kind, started);
                }
                update = update_rx.recv() => {
                    let Some(update) = update else {
                        break;
                    };
                    let kind = update_kind(&update);
                    let started = Instant::now();
                    let closed = apply_update(
                        client_id,
                        update,
                        secrets.as_ref(),
                        &source,
                        &events,
                        &mut live,
                        &requested_view,
                    )
                    .await;
                    note_slow_step("update", kind, started);
                    if closed {
                        break;
                    }
                }
            }
        }

        // Drop the command channel first: a command sent after the event below
        // then fails here and starts a new client, instead of dying in this queue.
        drop(commands);
        dispatcher.unregister(client_id);
        mark_done(&done);
        if live.ended_elsewhere {
            emit_telegram_session_ended(&events);
        }
    });

    cmd_tx
}

/// A worker step that takes this long or longer is logged. The worker
/// handles one command or one update at a time, so a slow step holds every
/// later update: a sent message stays on "Sending…" and an edit waits.
const SLOW_STEP: Duration = Duration::from_secs(1);

/// TDLib's own log writes fatal errors only. Its error lines can print a
/// whole message with its text, and no log may hold message text.
const TDLIB_LOG_LEVEL: i32 = 0;

/// Log a worker step that took [`SLOW_STEP`] or more. The line has the kind
/// of the step and the time only: no chat, no message, and no text.
fn note_slow_step(step: &'static str, kind: &'static str, started: Instant) {
    let took = started.elapsed();
    if took >= SLOW_STEP {
        let ms = u64::try_from(took.as_millis()).unwrap_or(u64::MAX);
        tracing::info!(step, kind, ms, "telegram worker step was slow");
    }
}

/// The name of an update for a log line. The kinds with a handler that waits
/// for TDLib have their own name. Every other kind is "other": its time is
/// the topic list pass after it.
fn update_kind(update: &tdlib_rs::enums::Update) -> &'static str {
    use tdlib_rs::enums::Update;
    match update {
        Update::AuthorizationState(_) => "authorization_state",
        Update::User(_) => "user",
        Update::NewMessage(_) => "new_message",
        Update::ForumTopicInfo(_) => "forum_topic_info",
        Update::ForumTopic(_) => "forum_topic",
        Update::File(_) => "file",
        _ => "other",
    }
}

/// Wait until every closing worker exited. `false` after [`RETIRE_TIMEOUT`].
async fn wait_for_retired(workers: &[DoneFlag]) -> bool {
    let deadline = Instant::now() + RETIRE_TIMEOUT;
    while !all_done(workers) {
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(WORKER_POLL).await;
    }
    true
}

/// Next command, or `None` once the sender is gone. Never resolves after that.
async fn next_command(
    commands: &mut Option<UnboundedReceiver<TdlibCommand>>,
) -> Option<TdlibCommand> {
    match commands.as_mut() {
        Some(commands) => commands.recv().await,
        None => std::future::pending().await,
    }
}

async fn request_log_out(client_id: i32) {
    if let Err(error) = tdlib_rs::functions::log_out(client_id).await {
        log_tdlib_error("logOut", &error);
        request_close(client_id).await;
    }
}

async fn request_close(client_id: i32) {
    if let Err(error) = tdlib_rs::functions::close(client_id).await {
        log_tdlib_error("close", &error);
    }
}

async fn apply_step(
    client_id: i32,
    step: TelegramAuthStep,
    secrets: &dyn TelegramSecretVault,
    events: &EventTx,
    closing: &ClosingFlag,
) {
    let login = LoginEvents::new(events, closing);
    match step {
        TelegramAuthStep::ApiCredentials => {
            login.status(
                AdapterStatus::Connecting,
                "TDLib client started. Waiting for authorization state.",
            );
        }
        TelegramAuthStep::Phone => {
            let Some(phone) = secrets.get_secret(TelegramSecretKey::Phone) else {
                login.phase(TelegramAuthPhase::Failed);
                login.status(
                    AdapterStatus::Error,
                    "Phone number is missing from the secret store.",
                );
                return;
            };
            if let Err(error) =
                tdlib_rs::functions::set_authentication_phone_number(phone, None, client_id).await
            {
                reject_step(&login, &error);
                login.status(
                    AdapterStatus::Error,
                    "Telegram rejected the phone number. Check the number or Cancel.",
                );
            }
        }
        TelegramAuthStep::ResendCode => {
            // Valid in WaitCode once the server timeout passed. A refusal comes
            // back as a normal rejected step with its error.
            if let Err(error) = tdlib_rs::functions::resend_authentication_code(
                Some(tdlib_rs::enums::ResendCodeReason::UserRequest),
                client_id,
            )
            .await
            {
                reject_step(&login, &error);
            }
        }
        TelegramAuthStep::Code => {
            let Some(code) = secrets.get_secret(TelegramSecretKey::Code) else {
                login.phase(TelegramAuthPhase::Failed);
                login.status(
                    AdapterStatus::Error,
                    "Login code is missing from the secret store.",
                );
                return;
            };
            if let Err(error) =
                tdlib_rs::functions::check_authentication_code(code, client_id).await
            {
                reject_step(&login, &error);
                login.status(
                    AdapterStatus::Error,
                    "Telegram rejected the login code. Try again or Cancel.",
                );
            }
        }
        TelegramAuthStep::TwoFactor | TelegramAuthStep::Complete => {
            let password = secrets
                .get_secret(TelegramSecretKey::Password)
                .unwrap_or_default();
            if password.is_empty() {
                return;
            }
            if let Err(error) =
                tdlib_rs::functions::check_authentication_password(password, client_id).await
            {
                reject_step(&login, &error);
                login.status(
                    AdapterStatus::Error,
                    "Telegram rejected the 2FA password. Try again or Cancel.",
                );
            }
        }
    }
}

/// Report why TDLib refused a login step, then the `Failed` phase.
/// Only the error name and code are read; the typed value is never echoed.
fn reject_step(login: &LoginEvents<'_>, error: &tdlib_rs::types::Error) {
    log_tdlib_error("login step", error);
    let reason: TelegramAuthError =
        super::auth_error::auth_error_from_tdlib(error.code, &error.message);
    login.rejected(reason);
    login.phase(TelegramAuthPhase::Failed);
}

fn code_via(kind: &tdlib_rs::enums::AuthenticationCodeType) -> TelegramCodeVia {
    use tdlib_rs::enums::AuthenticationCodeType as Kind;
    match kind {
        Kind::TelegramMessage(_) => TelegramCodeVia::TelegramApp,
        Kind::Sms(_) => TelegramCodeVia::Sms,
        Kind::SmsWord(_) | Kind::SmsPhrase(_) => TelegramCodeVia::SmsWord,
        Kind::Call(_) | Kind::FlashCall(_) | Kind::MissedCall(_) => TelegramCodeVia::Call,
        _ => TelegramCodeVia::Other,
    }
}

/// Returns `true` once TDLib reports `authorizationStateClosed`.
async fn apply_update(
    client_id: i32,
    update: tdlib_rs::enums::Update,
    secrets: &dyn TelegramSecretVault,
    source: &TelegramApiSource,
    events: &EventTx,
    live: &mut LiveInbox,
    requested_view: &Mutex<Option<String>>,
) -> bool {
    match update {
        tdlib_rs::enums::Update::AuthorizationState(state) => {
            let closed = matches!(
                state.authorization_state,
                tdlib_rs::enums::AuthorizationState::Closed
            );
            let ready = matches!(
                state.authorization_state,
                tdlib_rs::enums::AuthorizationState::Ready
            );
            apply_authorization(
                client_id,
                state.authorization_state,
                secrets,
                source,
                events,
                live,
            )
            .await;
            if ready && live.authorized {
                apply_requested_view(requested_view, &live.viewed, &mut live.open_chat);
                load_due_forum_topics(client_id, live, events).await;
            }
            return closed;
        }
        tdlib_rs::enums::Update::User(update) => {
            live.names.remember_user(
                update.user.id,
                &update.user.first_name,
                &update.user.last_name,
            );
            if let tdlib_rs::enums::UserType::Bot(bot) = &update.user.r#type {
                let effects = live
                    .directory
                    .note_topic_bot(update.user.id, bot.has_topics);
                let emit = live.linked();
                for effect in effects {
                    live.publish(events, emit, Some(effect));
                }
            }
            load_due_forum_topics(client_id, live, events).await;
        }
        other => {
            // `view_chat` published the cell while this update waited behind
            // `getChatHistory`. `open_chat` is still the previous command.
            live.open_chat = live.viewed.get();
            let mark_read = live_read_while_linked(live, &other);
            {
                let mut unviewed = live.unviewed.lock().unwrap_or_else(PoisonError::into_inner);
                remember_unviewed(&other, live.open_chat, &mut unviewed);
            }
            apply_chat_update(client_id, other, live, events).await;
            load_due_forum_topics(client_id, live, events).await;
            if let Some((chat_id, message_id)) = mark_read {
                // A round trip here would stall every later update.
                tokio::spawn(mark_message_viewed(client_id, chat_id, message_id));
            }
        }
    }
    false
}

/// `viewMessages` for a live message. A client that is closing, or whose
/// login epoch moved, is not linked and must not mark anything read.
fn live_read_while_linked(
    live: &LiveInbox,
    update: &tdlib_rs::enums::Update,
) -> Option<(i64, i64)> {
    if !live.linked() {
        return None;
    }
    live_read_in_topic(update, live.open_chat, live.open_topic)
}

/// A live incoming message in the open chat, with no topic open.
/// Tests keep this two-argument form. The worker uses [`live_read_in_topic`].
#[cfg(test)]
fn live_read_of(update: &tdlib_rs::enums::Update, open_chat: Option<i64>) -> Option<(i64, i64)> {
    live_read_in_topic(update, open_chat, None)
}

fn live_read_in_topic(
    update: &tdlib_rs::enums::Update,
    open_chat: Option<i64>,
    open_topic: Option<MessageTopic>,
) -> Option<(i64, i64)> {
    let tdlib_rs::enums::Update::NewMessage(update) = update else {
        return None;
    };
    let message = &update.message;
    if message.is_outgoing {
        return None;
    }
    let message_topic = message.topic_id.as_ref().and_then(app_topic);
    if message_topic != open_topic {
        return None;
    }
    inbox::live_message_to_view(open_chat, message.chat_id, message.id)
}

/// `ViewChat` names the chat the user looks at. `None`, or an id that is
/// not a Telegram chat, leaves no chat open for live `viewMessages`.
fn apply_viewed_chat(open_chat: &mut Option<i64>, conversation_id: Option<&str>) {
    *open_chat = inbox::viewed_chat_id(conversation_id);
}

/// Apply a queued `ViewChat`. A stamp that lost the race does not write its
/// chat back; the worker keeps whatever the cell already published.
fn apply_view_command(
    viewed: &inbox::ViewedChat,
    open_chat: &mut Option<i64>,
    open_topic: &mut Option<MessageTopic>,
    conversation_id: Option<&str>,
    stamp: u64,
) {
    let chat = inbox::viewed_chat_id(conversation_id);
    if viewed.sync(stamp, chat) {
        apply_viewed_chat(open_chat, conversation_id);
        *open_topic = None;
    } else {
        *open_chat = viewed.get();
        *open_topic = viewed.get_topic();
    }
}

/// An incoming message in a chat the user is not looking at. The latest id
/// is enough: `viewMessages` with `force_read` marks through that id.
fn remember_unviewed(
    update: &tdlib_rs::enums::Update,
    open_chat: Option<i64>,
    unviewed: &mut HashMap<i64, i64>,
) {
    let tdlib_rs::enums::Update::NewMessage(update) = update else {
        return;
    };
    let message = &update.message;
    if message.is_outgoing || open_chat == Some(message.chat_id) {
        return;
    }
    remember_id(unviewed, message.chat_id, message.id);
}

/// Keep the newest id for this chat. A later id replaces an earlier one.
fn remember_id(unviewed: &mut HashMap<i64, i64>, chat_id: i64, message_id: i64) {
    let slot = unviewed.entry(chat_id).or_insert(message_id);
    if message_id > *slot {
        *slot = message_id;
    }
}

/// What a history page does with its ids. `Mark` calls `viewMessages` now.
/// `Remember` keeps an id for the next focus, or stores nothing.
enum HistoryPageRead {
    /// The open chat, with no topic. `newest` is kept if `viewMessages` fails.
    Mark { newest: i64 },
    /// Not marked now. `Some` waits for focus. `None` stores nothing: the
    /// page is empty, or a topic of this chat is open.
    Remember(Option<i64>),
}

/// Mark this page now, or keep its newest id for focus (#174 item 12).
fn history_page_read(
    viewed: Option<i64>,
    viewed_topic: Option<MessageTopic>,
    chat_id: i64,
    message_ids: &[i64],
) -> HistoryPageRead {
    let newest = message_ids.iter().copied().max();
    if inbox::should_mark_history_viewed(viewed, viewed_topic, chat_id)
        && let Some(newest) = newest
    {
        HistoryPageRead::Mark { newest }
    } else if viewed == Some(chat_id) {
        HistoryPageRead::Remember(None)
    } else {
        HistoryPageRead::Remember(newest)
    }
}

/// `ViewChat` selected this chat again. Returns the id to pass to
/// `viewMessages`, if one is waiting. The id stays until
/// [`note_view_succeeded`]. A failed call leaves it for the next focus
/// (#174 item 13).
fn catch_up_read(open_chat: Option<i64>, unviewed: &HashMap<i64, i64>) -> Option<(i64, i64)> {
    let chat_id = open_chat?;
    let message_id = *unviewed.get(&chat_id)?;
    Some((chat_id, message_id))
}

/// `viewMessages` for `message_id` succeeded. Drop it. A newer id that
/// arrived while the call was in flight stays.
fn note_view_succeeded(unviewed: &mut HashMap<i64, i64>, chat_id: i64, message_id: i64) {
    if unviewed
        .get(&chat_id)
        .is_some_and(|saved| *saved <= message_id)
    {
        unviewed.remove(&chat_id);
    }
}

/// Ready copies the last `ViewChat` into this worker's cell. `sync_viewed`
/// does not send the same chat again after a restart, and a request that
/// arrived with no worker was not on the command channel.
fn apply_requested_view(
    requested: &Mutex<Option<String>>,
    viewed: &inbox::ViewedChat,
    open_chat: &mut Option<i64>,
) {
    let requested = requested.lock().unwrap_or_else(PoisonError::into_inner);
    let chat = inbox::viewed_chat_id(requested.as_deref());
    // Still holding `requested`. A blur cannot store None and then lose
    // the race to this publish.
    let _ = viewed.publish(chat);
    *open_chat = chat;
}

async fn mark_message_viewed(client_id: i32, chat_id: i64, message_id: i64) -> bool {
    match tdlib_rs::functions::view_messages(chat_id, vec![message_id], None, true, client_id).await
    {
        Ok(_) => true,
        Err(error) => {
            log_tdlib_error("viewMessages", &error);
            false
        }
    }
}

async fn apply_authorization(
    client_id: i32,
    state: tdlib_rs::enums::AuthorizationState,
    secrets: &dyn TelegramSecretVault,
    source: &TelegramApiSource,
    events: &EventTx,
    live: &mut LiveInbox,
) {
    let login = LoginEvents::new(events, &live.closing);
    match state {
        tdlib_rs::enums::AuthorizationState::WaitTdlibParameters => {
            set_parameters(client_id, secrets, source, events, &live.closing).await;
        }
        tdlib_rs::enums::AuthorizationState::WaitPhoneNumber => {
            live.new_login = true;
            // A saved session that lands here expired or was revoked. Drop the
            // marker so the next launch does not try to resume it again.
            if secrets.get_secret(TelegramSecretKey::Session).is_some() {
                secrets.set_secret(TelegramSecretKey::Session, "");
                super::request_secret_flush(events);
            }
            login.phase(TelegramAuthPhase::NeedPhone);
            emit_status(
                events,
                ProtocolId::Telegram,
                AdapterStatus::Connecting,
                "Telegram needs a phone number. Values stay in the secret store.",
            );
        }
        tdlib_rs::enums::AuthorizationState::WaitCode(state) => {
            live.new_login = true;
            login.code_sent(code_via(&state.code_info.r#type));
            login.phase(TelegramAuthPhase::NeedCode);
            emit_status(
                events,
                ProtocolId::Telegram,
                AdapterStatus::Connecting,
                "Telegram sent a login code. Enter it here. It is not logged.",
            );
        }
        tdlib_rs::enums::AuthorizationState::WaitPassword(_) => {
            live.new_login = true;
            login.phase(TelegramAuthPhase::NeedTwoFactor);
            emit_status(
                events,
                ProtocolId::Telegram,
                AdapterStatus::Connecting,
                "Telegram needs the account password (two-step verification).",
            );
        }
        tdlib_rs::enums::AuthorizationState::Ready => {
            // A client that is closing, or whose epoch Cancel moved, does not
            // link: no marker, no inbox. Checked before each side effect.
            if !login.open() {
                // TDLib is signed in, but the UI never links it. A cancelled
                // new login must not stay signed in (PR #49 review).
                match late_ready(live.new_login, live.close_cancel) {
                    LateReady::Wait => live.late_ready = true,
                    LateReady::LogOut => request_log_out(client_id).await,
                    LateReady::Keep => {}
                }
                return;
            }
            secrets.set_secret(TelegramSecretKey::Session, TDLIB_SESSION_MARKER);
            super::request_secret_flush(events);
            live.authorized = true;
            login.phase(TelegramAuthPhase::Ready);
            emit_status(
                events,
                ProtocolId::Telegram,
                AdapterStatus::Ready,
                "Telegram is ready. Loading the chat list.",
            );
            if !login.open() {
                return;
            }
            for conversation in live.directory.listed() {
                emit_conversation(events, conversation);
            }
            if !login.open() {
                return;
            }
            load_main_chats(client_id, true, events).await;
        }
        tdlib_rs::enums::AuthorizationState::WaitEmailAddress(_)
        | tdlib_rs::enums::AuthorizationState::WaitEmailCode(_)
        | tdlib_rs::enums::AuthorizationState::WaitRegistration(_)
        | tdlib_rs::enums::AuthorizationState::WaitPremiumPurchase(_)
        | tdlib_rs::enums::AuthorizationState::WaitOtherDeviceConfirmation(_) => {
            emit_status(
                events,
                ProtocolId::Telegram,
                AdapterStatus::Error,
                "Telegram asked for a login step thinwire does not support yet. Cancel and use another client.",
            );
        }
        tdlib_rs::enums::AuthorizationState::LoggingOut
        | tdlib_rs::enums::AuthorizationState::Closing
        | tdlib_rs::enums::AuthorizationState::Closed => {
            let was_authorized = std::mem::replace(&mut live.authorized, false);
            // A live session that closes without our request ended elsewhere,
            // for example Settings → Devices on another client (qa R73). The
            // event goes out only after Closed, when the worker has exited (R74).
            if was_authorized && !live.closing.is_set() {
                live.ended_elsewhere = true;
            }
            live.open_chat = None;
            live.viewed.clear();
            emit_status(
                events,
                ProtocolId::Telegram,
                AdapterStatus::Stubbed,
                "Telegram session closed.",
            );
        }
    }
}

async fn apply_chat_update(
    client_id: i32,
    update: tdlib_rs::enums::Update,
    live: &mut LiveInbox,
    events: &EventTx,
) {
    // No inbox event from a closing client or a cancelled epoch (PR #49 review).
    let emit = live.linked();
    match update {
        tdlib_rs::enums::Update::NewChat(update) => {
            let chat_id = update.chat.id;
            let effect = note_chat(&mut live.directory, &update.chat);
            live.publish(events, emit, effect);
            let last_read = live.directory.last_read_outbox(chat_id);
            if emit && last_read > 0 {
                emit_outbox_read(
                    events,
                    ProtocolId::Telegram,
                    inbox::conversation_id(chat_id),
                    last_read,
                );
            }
        }
        tdlib_rs::enums::Update::ChatTitle(update) => {
            let effect = live.directory.set_title(update.chat_id, &update.title);
            live.publish(events, emit, effect);
        }
        tdlib_rs::enums::Update::ChatPhoto(update) => {
            // Drop the cached file id, then store the new small photo.
            let effect = live
                .directory
                .set_photo(update.chat_id, small_photo_id(update.photo.as_ref()));
            live.publish(events, emit, effect);
        }
        tdlib_rs::enums::Update::ChatAccentColors(update) => {
            let effect = live
                .directory
                .set_accent(update.chat_id, update.accent_color_id);
            live.publish(events, emit, effect);
        }
        tdlib_rs::enums::Update::ChatPosition(update) => {
            if matches!(update.position.list, tdlib_rs::enums::ChatList::Main) {
                let effect = live
                    .directory
                    .set_main_order(update.chat_id, update.position.order);
                live.publish(events, emit, effect);
            }
        }
        tdlib_rs::enums::Update::ChatNotificationSettings(update) => {
            let effect = live
                .directory
                .set_mute(update.chat_id, chat_mute(&update.notification_settings));
            live.publish(events, emit, effect);
        }
        tdlib_rs::enums::Update::ScopeNotificationSettings(update) => {
            let muted = update.notification_settings.mute_for > 0;
            let effects = live
                .directory
                .set_scope_mute(mute_scope(&update.scope), muted);
            for effect in effects {
                live.publish(events, emit, Some(effect));
            }
        }
        tdlib_rs::enums::Update::ChatReadInbox(update) => {
            let effect = live
                .directory
                .set_unread(update.chat_id, update.unread_count);
            live.publish(events, emit, effect);
        }
        tdlib_rs::enums::Update::ChatReadOutbox(update) => {
            // Ids and the link flag only. No title and no message text.
            tracing::debug!(
                update = "ChatReadOutbox",
                chat_id = update.chat_id,
                old_message_id = Option::<i64>::None,
                message_id = update.last_read_outbox_message_id,
                sending_state = "absent",
                emit,
                "telegram outgoing update"
            );
            // Not emitted while the account is not linked. NewChat keeps the
            // cursor the same way. The directory stores it. Rows loaded later
            // get Read. Rows already in the snapshot update on the next
            // ChatReadOutbox.
            if live
                .directory
                .note_outbox_read(update.chat_id, update.last_read_outbox_message_id)
                && emit
                && update.last_read_outbox_message_id > 0
            {
                emit_outbox_read(
                    events,
                    ProtocolId::Telegram,
                    inbox::conversation_id(update.chat_id),
                    update.last_read_outbox_message_id,
                );
            }
        }
        tdlib_rs::enums::Update::ChatLastMessage(update) => {
            // `positions` is the full list. No Main entry means the chat left
            // the main list (for example it was archived).
            let effect = live
                .directory
                .set_main_position(update.chat_id, main_order(&update.positions));
            live.publish(events, emit, effect);
            if let Some(message) = update.last_message.as_ref() {
                let preview = super::media_map::map_content(&message.content).preview;
                let effect =
                    live.directory
                        .set_preview(update.chat_id, &preview, i64::from(message.date));
                live.publish(events, emit, effect);
                if emit {
                    emit_mapped_message(
                        client_id,
                        events,
                        message,
                        live,
                        None,
                        OutgoingUpdate::Listed,
                    );
                }
            } else {
                let effect = live.directory.set_preview(update.chat_id, "", 0);
                live.publish(events, emit, effect);
            }
        }
        tdlib_rs::enums::Update::NewMessage(update) => {
            let preview = super::media_map::map_content(&update.message.content).preview;
            let chat_id = update.message.chat_id;
            let at = i64::from(update.message.date);
            if emit {
                // A topic that is not loaded yet is fetched before the message,
                // so a mute and the "Group › Topic" title are known. The core
                // waits when that row is not applied yet.
                if let Some(MessageTopic::Forum(id)) =
                    update.message.topic_id.as_ref().and_then(app_topic)
                    && let Ok(forum_topic_id) = i32::try_from(id)
                    && !live.topics.contains_key(&(chat_id, forum_topic_id))
                {
                    refresh_forum_topic(client_id, chat_id, forum_topic_id, live, events).await;
                }
                emit_mapped_message_as(
                    client_id,
                    events,
                    &update.message,
                    live,
                    None,
                    OutgoingUpdate::Listed,
                    Arrival::Live,
                );
            }
            let effect = live.directory.set_preview(chat_id, &preview, at);
            live.publish(events, emit, effect);
        }
        // The request reached the server. The temporary id stays, and the
        // row stays pending. This update is sent only when `use_quick_ack`
        // is on. Success and failure are the updates below.
        tdlib_rs::enums::Update::MessageSendAcknowledged(update) => {
            tracing::debug!(
                update = "MessageSendAcknowledged",
                chat_id = update.chat_id,
                old_message_id = Option::<i64>::None,
                message_id = update.message_id,
                sending_state = "absent",
                emit,
                "telegram outgoing update"
            );
        }
        tdlib_rs::enums::Update::MessageSendSucceeded(update) => {
            tracing::debug!(
                update = "MessageSendSucceeded",
                chat_id = update.message.chat_id,
                old_message_id = Some(update.old_message_id),
                message_id = update.message.id,
                sending_state = sending_state_name(&update.message.sending_state),
                emit,
                "telegram outgoing update"
            );
            if emit {
                let old_id = inbox::message_id(update.message.chat_id, update.old_message_id);
                emit_mapped_message(
                    client_id,
                    events,
                    &update.message,
                    live,
                    Some(old_id),
                    OutgoingUpdate::Succeeded,
                );
                emit_status(
                    events,
                    ProtocolId::Telegram,
                    AdapterStatus::Ready,
                    "Message sent.",
                );
            }
        }
        tdlib_rs::enums::Update::MessageSendFailed(update) => {
            tracing::debug!(
                update = "MessageSendFailed",
                chat_id = update.message.chat_id,
                old_message_id = Some(update.old_message_id),
                message_id = update.message.id,
                sending_state = sending_state_name(&update.message.sending_state),
                emit,
                "telegram outgoing update"
            );
            if emit {
                log_tdlib_error("sendMessage (update)", &update.error);
                let old_id = inbox::message_id(update.message.chat_id, update.old_message_id);
                emit_mapped_message(
                    client_id,
                    events,
                    &update.message,
                    live,
                    Some(old_id),
                    OutgoingUpdate::Failed,
                );
                emit_status(
                    events,
                    ProtocolId::Telegram,
                    AdapterStatus::Error,
                    format!(
                        "Telegram did not send the message (TDLib {}).",
                        update.error.code
                    ),
                );
            }
        }
        tdlib_rs::enums::Update::MessageInteractionInfo(update) => {
            if !emit {
                return;
            }
            let _ = events.send(AdapterEvent::MessageReactions {
                protocol: ProtocolId::Telegram,
                conversation_id: inbox::conversation_id(update.chat_id),
                message_id: inbox::message_id(update.chat_id, update.message_id),
                reactions: super::media_map::reactions_of(update.interaction_info.as_ref()),
            });
        }
        tdlib_rs::enums::Update::DeleteMessages(update) => {
            // Cache eviction can be fetched again. Rows drop only when the chat lost them.
            if update.from_cache {
                return;
            }
            live.older.messages_deleted(update.chat_id);
            if !emit {
                return;
            }
            let message_ids = update
                .message_ids
                .iter()
                .map(|message_id| inbox::message_id(update.chat_id, *message_id))
                .collect();
            emit_messages_removed(
                events,
                ProtocolId::Telegram,
                inbox::conversation_id(update.chat_id),
                message_ids,
            );
        }
        tdlib_rs::enums::Update::MessageContent(update) => {
            if emit {
                emit_content(
                    client_id,
                    events,
                    update.chat_id,
                    update.message_id,
                    &update.new_content,
                );
            }
        }
        tdlib_rs::enums::Update::MessageEdited(update) => {
            // The new text came in `updateMessageContent` just before.
            if emit {
                emit_message_edited(
                    events,
                    ProtocolId::Telegram,
                    inbox::conversation_id(update.chat_id),
                    inbox::message_id(update.chat_id, update.message_id),
                );
            }
        }
        tdlib_rs::enums::Update::ChatViewAsTopics(update) => {
            let effect = live
                .directory
                .set_view_as_topics(update.chat_id, update.view_as_topics);
            live.publish(events, emit, effect);
        }
        tdlib_rs::enums::Update::ForumTopicInfo(update) => {
            let key = (update.info.chat_id, update.info.forum_topic_id);
            if let Some(topic) = live.topics.get_mut(&key) {
                topic.name.clone_from(&update.info.name);
                topic.general = update.info.is_general;
                topic.hidden = update.info.is_hidden;
                topic.closed = update.info.is_closed;
                if emit {
                    emit_topic(events, topic.clone());
                }
            } else if emit && live.topic_lists.is_loaded(update.info.chat_id) {
                // A new topic of a loaded forum. A forum with no list gets
                // this topic from its list load.
                refresh_forum_topic(
                    client_id,
                    update.info.chat_id,
                    update.info.forum_topic_id,
                    live,
                    events,
                )
                .await;
            }
        }
        tdlib_rs::enums::Update::ForumTopic(update) => {
            // This update carries the mute. Unread and the last message come
            // from `getForumTopic`. TDLib sends this update for each topic of
            // each forum at start, and a fetch for each one held the update
            // queue for minutes. So fetch only a topic whose read mark moved,
            // or a new topic of a forum whose list loaded.
            if emit
                && live.topic_lists.needs_fetch(
                    update.chat_id,
                    update.forum_topic_id,
                    update.last_read_inbox_message_id,
                )
            {
                refresh_forum_topic(
                    client_id,
                    update.chat_id,
                    update.forum_topic_id,
                    live,
                    events,
                )
                .await;
            }
            let key = (update.chat_id, update.forum_topic_id);
            if let Some(topic) = live.topics.get_mut(&key) {
                let muted = update.notification_settings.mute_for > 0;
                if topic.muted != muted {
                    topic.muted = muted;
                    if emit {
                        emit_topic(events, topic.clone());
                    }
                }
            }
        }
        tdlib_rs::enums::Update::File(update) => {
            note_file(client_id, update.file, live, events).await;
        }
        tdlib_rs::enums::Update::ChatAction(update) if emit => {
            let _ = events.send(typing_event(&update, &live.names, &live.directory));
        }
        _ => {}
    }
}

/// The TDLib chat and topic of a `SetTyping`. `None` when the id is not a
/// Telegram chat, or the topic has no TDLib form: the state then goes to
/// no chat, not to the whole chat.
fn typing_target(
    conversation_id: &str,
    topic: Option<MessageTopic>,
) -> Option<(i64, Option<tdlib_rs::enums::MessageTopic>)> {
    let chat_id = inbox::parse_telegram_chat_id(conversation_id)?;
    match topic {
        None => Some((chat_id, None)),
        Some(topic) => Some((chat_id, Some(td_topic(topic)?))),
    }
}

/// `sendChatAction`: Typing, or Cancel to stop. Telegram ends the action by
/// itself after some seconds.
async fn send_typing(
    client_id: i32,
    chat_id: i64,
    topic: Option<tdlib_rs::enums::MessageTopic>,
    typing: bool,
) {
    let action = if typing {
        tdlib_rs::enums::ChatAction::Typing
    } else {
        tdlib_rs::enums::ChatAction::Cancel
    };
    if let Err(error) =
        tdlib_rs::functions::send_chat_action(chat_id, topic, Some(action), client_id).await
    {
        log_tdlib_error("sendChatAction", &error);
    }
}

/// The `Typing` event of a TDLib chat action. Only "typing a message"
/// counts. Another action (a voice note, a photo upload) or Cancel ends the
/// typing state of that person.
fn typing_event(
    update: &tdlib_rs::types::UpdateChatAction,
    names: &NameBook,
    directory: &ChatDirectory,
) -> AdapterEvent {
    let (sender_id, sender) = match &update.sender_id {
        tdlib_rs::enums::MessageSender::User(user) => (
            format!("user:{}", user.user_id),
            names.user_label(user.user_id),
        ),
        tdlib_rs::enums::MessageSender::Chat(chat) => (
            format!("chat:{}", chat.chat_id),
            directory
                .title(chat.chat_id)
                .unwrap_or("Telegram chat")
                .to_owned(),
        ),
    };
    AdapterEvent::Typing {
        protocol: ProtocolId::Telegram,
        conversation_id: inbox::conversation_id(update.chat_id),
        topic: update.topic_id.as_ref().and_then(app_topic),
        sender_id,
        sender,
        typing: matches!(update.action, tdlib_rs::enums::ChatAction::Typing),
    }
}

fn emit_effect(events: &EventTx, effect: Option<ChatEffect>) {
    match effect {
        Some(ChatEffect::Upsert(conversation)) => emit_conversation(events, conversation),
        Some(ChatEffect::Remove(id)) => {
            emit_conversation_removed(events, ProtocolId::Telegram, id);
        }
        None => {}
    }
}

/// A forum supergroup shown as topics, or a private chat with a topic bot.
/// Saved Messages is a private chat with `view_as_topics`, so it stays a chat.
fn forum_or_bot(
    directory: &ChatDirectory,
    kind: &tdlib_rs::enums::ChatType,
    view_as_topics: bool,
) -> bool {
    match kind {
        tdlib_rs::enums::ChatType::Supergroup(group) => view_as_topics && !group.is_channel,
        tdlib_rs::enums::ChatType::Private(chat) => directory.bot_has_topics(chat.user_id),
        tdlib_rs::enums::ChatType::BasicGroup(_) | tdlib_rs::enums::ChatType::Secret(_) => false,
    }
}

fn chat_peer(kind: &tdlib_rs::enums::ChatType) -> Option<i64> {
    match kind {
        tdlib_rs::enums::ChatType::Private(chat) => Some(chat.user_id),
        tdlib_rs::enums::ChatType::BasicGroup(_)
        | tdlib_rs::enums::ChatType::Supergroup(_)
        | tdlib_rs::enums::ChatType::Secret(_) => None,
    }
}

fn app_topic(topic: &tdlib_rs::enums::MessageTopic) -> Option<MessageTopic> {
    match topic {
        tdlib_rs::enums::MessageTopic::Forum(topic) => {
            Some(MessageTopic::Forum(i64::from(topic.forum_topic_id)))
        }
        tdlib_rs::enums::MessageTopic::Thread(topic) => {
            Some(MessageTopic::Thread(topic.message_thread_id))
        }
        tdlib_rs::enums::MessageTopic::DirectMessages(_)
        | tdlib_rs::enums::MessageTopic::SavedMessages(_) => None,
    }
}

fn td_topic(topic: MessageTopic) -> Option<tdlib_rs::enums::MessageTopic> {
    match topic {
        MessageTopic::Forum(id) => {
            let forum_topic_id = i32::try_from(id).ok()?;
            Some(tdlib_rs::enums::MessageTopic::Forum(
                tdlib_rs::types::MessageTopicForum { forum_topic_id },
            ))
        }
        MessageTopic::Thread(message_thread_id) => Some(tdlib_rs::enums::MessageTopic::Thread(
            tdlib_rs::types::MessageTopicThread { message_thread_id },
        )),
    }
}

fn note_chat(directory: &mut ChatDirectory, chat: &tdlib_rs::types::Chat) -> Option<ChatEffect> {
    let order = main_order(&chat.positions).unwrap_or(0);
    let preview = chat
        .last_message
        .as_ref()
        .map(|message| super::media_map::map_content(&message.content).preview)
        .unwrap_or_default();
    let last_at = chat
        .last_message
        .as_ref()
        .map_or(0, |message| i64::from(message.date));
    let is_group = match &chat.r#type {
        tdlib_rs::enums::ChatType::BasicGroup(_) => true,
        tdlib_rs::enums::ChatType::Supergroup(group) => !group.is_channel,
        tdlib_rs::enums::ChatType::Private(_) | tdlib_rs::enums::ChatType::Secret(_) => false,
    };
    let scope = match &chat.r#type {
        tdlib_rs::enums::ChatType::Private(_) | tdlib_rs::enums::ChatType::Secret(_) => {
            MuteScope::Private
        }
        tdlib_rs::enums::ChatType::BasicGroup(_) => MuteScope::Group,
        tdlib_rs::enums::ChatType::Supergroup(group) if group.is_channel => MuteScope::Channel,
        tdlib_rs::enums::ChatType::Supergroup(_) => MuteScope::Group,
    };
    directory.upsert(
        chat.id,
        ChatSeed {
            title: &chat.title,
            order,
            unread: chat.unread_count,
            preview: &preview,
            participant: &chat.title,
            last_at,
            is_group,
            scope,
            mute: chat_mute(&chat.notification_settings),
            has_topics: forum_or_bot(directory, &chat.r#type, chat.view_as_topics),
            peer: chat_peer(&chat.r#type),
            photo_file_id: small_photo_id(chat.photo.as_ref()),
            accent: crate::media::ChatAvatar::accent_index(chat.accent_color_id),
            last_read_outbox: chat.last_read_outbox_message_id,
        },
    )
}

/// File id of `chat.photo.small` (160×160). The animated variant is ignored.
fn small_photo_id(photo: Option<&tdlib_rs::types::ChatPhotoInfo>) -> Option<i32> {
    photo.map(|photo| photo.small.id)
}

fn chat_mute(settings: &tdlib_rs::types::ChatNotificationSettings) -> ChatMute {
    ChatMute {
        use_default: settings.use_default_mute_for,
        mute_for: settings.mute_for,
    }
}

fn mute_scope(scope: &tdlib_rs::enums::NotificationSettingsScope) -> MuteScope {
    match scope {
        tdlib_rs::enums::NotificationSettingsScope::PrivateChats => MuteScope::Private,
        tdlib_rs::enums::NotificationSettingsScope::GroupChats => MuteScope::Group,
        tdlib_rs::enums::NotificationSettingsScope::ChannelChats => MuteScope::Channel,
    }
}

fn main_order(positions: &[tdlib_rs::types::ChatPosition]) -> Option<i64> {
    positions
        .iter()
        .find(|position| matches!(position.list, tdlib_rs::enums::ChatList::Main))
        .map(|position| position.order)
}

fn emit_mapped_message(
    client_id: i32,
    events: &EventTx,
    message: &tdlib_rs::types::Message,
    live: &LiveInbox,
    replace_old: Option<String>,
    update: OutgoingUpdate,
) {
    emit_mapped_message_as(
        client_id,
        events,
        message,
        live,
        replace_old,
        update,
        Arrival::History,
    );
}

/// `arrival` is `Live` only for `updateNewMessage`: a history page, a last
/// message refresh, and a send result never notify (#32).
fn emit_mapped_message_as(
    client_id: i32,
    events: &EventTx,
    message: &tdlib_rs::types::Message,
    live: &LiveInbox,
    replace_old: Option<String>,
    update: OutgoingUpdate,
    arrival: Arrival,
) {
    let party = match &message.sender_id {
        tdlib_rs::enums::MessageSender::User(user) => MessageParty::User(user.user_id),
        tdlib_rs::enums::MessageSender::Chat(chat) => MessageParty::Chat(chat.chat_id),
    };
    let sending = match &message.sending_state {
        None => Delivery::Sent,
        Some(tdlib_rs::enums::MessageSendingState::Pending(_)) => Delivery::Pending,
        Some(tdlib_rs::enums::MessageSendingState::Failed(_)) => Delivery::Failed,
    };
    let media = super::media_map::map_content(&message.content);
    let mapped = inbox::to_chat_message(
        &InboxMessage {
            chat_id: message.chat_id,
            message_id: message.id,
            outgoing: message.is_outgoing,
            party,
            body: media.text.clone(),
            sent_at: i64::from(message.date),
            delivery: outgoing_delivery(
                update,
                sending,
                message.is_outgoing,
                message.id,
                live.directory.last_read_outbox(message.chat_id),
            ),
            topic: message.topic_id.as_ref().and_then(app_topic),
        },
        &live.names,
        live.directory.title(message.chat_id),
    );
    let mut mapped = ChatMessage { arrival, ..mapped };
    mapped.links = media.links;
    mapped.photo = media.photo;
    mapped.audio = media.audio;
    mapped.video = media.video;
    mapped.file = media.file;
    mapped.reactions = super::media_map::reactions_of(message.interaction_info.as_ref());
    mapped.reply_to = super::media_map::reply_of(message);
    mapped.edited = message.edit_date != 0;
    if media.scan {
        schedule_text_entities(
            client_id,
            events.clone(),
            mapped.conversation_id.clone(),
            mapped.id.clone(),
            media.text,
        );
    }
    if let Some(old_id) = replace_old.filter(|old_id| *old_id != mapped.id) {
        emit_message_replaced(events, old_id, mapped);
        return;
    }
    emit_message(events, mapped);
}

fn schedule_text_entities(
    client_id: i32,
    events: EventTx,
    conversation_id: String,
    message_id: String,
    text: String,
) {
    tokio::spawn(async move {
        match tdlib_rs::functions::get_text_entities(text.clone(), client_id).await {
            Ok(tdlib_rs::enums::TextEntities::TextEntities(found)) => {
                let links = super::media_map::links_from(&text, &found.entities);
                if !links.is_empty() {
                    emit_message_links(
                        &events,
                        ProtocolId::Telegram,
                        conversation_id,
                        message_id,
                        links,
                    );
                }
            }
            Err(error) => log_tdlib_error("getTextEntities", &error),
        }
    });
}

async fn start_download(
    client_id: i32,
    file_id: i32,
    secret: bool,
    live: &mut LiveInbox,
    events: &EventTx,
) {
    begin_download(
        client_id,
        file_id,
        InFlight::Photo {
            secret,
            saw_active: false,
        },
        live,
        events,
    )
    .await;
}

async fn start_voice(client_id: i32, file_id: i32, live: &mut LiveInbox, events: &EventTx) {
    begin_download(
        client_id,
        file_id,
        InFlight::Voice { saw_active: false },
        live,
        events,
    )
    .await;
}

async fn begin_download(
    client_id: i32,
    file_id: i32,
    kind: InFlight,
    live: &mut LiveInbox,
    events: &EventTx,
) {
    if live.downloads.contains_key(&file_id) {
        return;
    }
    live.downloads.insert(file_id, kind);
    match tdlib_rs::functions::download_file(file_id, 1, 0, 0, false, client_id).await {
        Ok(tdlib_rs::enums::File::File(file)) => {
            if file.local.is_downloading_completed {
                live.downloads.remove(&file_id);
                finish_download(client_id, file, kind, events).await;
            } else if !file.local.is_downloading_active {
                live.downloads.remove(&file_id);
                fail_download(kind, file_id, events);
            } else if let Some(row) = live.downloads.get_mut(&file_id) {
                row.mark_active();
            }
        }
        Err(error) => {
            log_tdlib_error("downloadFile", &error);
            live.downloads.remove(&file_id);
            fail_download(kind, file_id, events);
        }
    }
}

async fn note_file(
    client_id: i32,
    file: tdlib_rs::types::File,
    live: &mut LiveInbox,
    events: &EventTx,
) {
    let Some(kind) = live.downloads.get(&file.id).copied() else {
        return;
    };
    if file.local.is_downloading_completed {
        live.downloads.remove(&file.id);
        finish_download(client_id, file, kind, events).await;
    } else if file.local.is_downloading_active {
        if let Some(row) = live.downloads.get_mut(&file.id) {
            row.mark_active();
        }
    } else if kind.saw_active() {
        live.downloads.remove(&file.id);
        fail_download(kind, file.id, events);
    }
}

async fn finish_download(
    client_id: i32,
    file: tdlib_rs::types::File,
    kind: InFlight,
    events: &EventTx,
) {
    match kind {
        InFlight::Photo { secret, .. } => finish_photo(client_id, file, secret, events).await,
        InFlight::Voice { .. } => finish_voice(file, events),
        InFlight::Video { .. } => {
            let _ = events.send(video_event(
                file.id,
                file.local.is_downloading_completed,
                &file.local.path,
            ));
        }
        InFlight::File { .. } => {
            let _ = events.send(file_event(
                file.id,
                file.local.is_downloading_completed,
                &file.local.path,
            ));
        }
    }
}

fn fail_download(kind: InFlight, file_id: i32, events: &EventTx) {
    let event = match kind {
        InFlight::Photo { .. } => {
            emit_photo_failed(events, ProtocolId::Telegram, file_id);
            return;
        }
        InFlight::Voice { .. } => voice_event(file_id, false, ""),
        InFlight::Video { .. } => video_event(file_id, false, ""),
        InFlight::File { .. } => file_event(file_id, false, ""),
    };
    let _ = events.send(event);
}

fn video_event(file_id: i32, completed: bool, path: &str) -> AdapterEvent {
    match crate::AudioPath::new(path).filter(|_| completed) {
        Some(path) => AdapterEvent::VideoReady {
            protocol: ProtocolId::Telegram,
            file_id,
            path,
        },
        None => AdapterEvent::VideoFailed {
            protocol: ProtocolId::Telegram,
            file_id,
        },
    }
}

fn finish_voice(file: tdlib_rs::types::File, events: &EventTx) {
    let event = voice_event(
        file.id,
        file.local.is_downloading_completed,
        &file.local.path,
    );
    let _ = events.send(event);
}

fn file_event(file_id: i32, completed: bool, path: &str) -> AdapterEvent {
    match crate::AudioPath::new(path).filter(|_| completed) {
        Some(path) => AdapterEvent::FileReady {
            protocol: ProtocolId::Telegram,
            file_id,
            path,
        },
        None => AdapterEvent::FileFailed {
            protocol: ProtocolId::Telegram,
            file_id,
        },
    }
}

fn voice_event(file_id: i32, completed: bool, path: &str) -> AdapterEvent {
    match crate::AudioPath::new(path).filter(|_| completed) {
        Some(path) => AdapterEvent::VoiceReady {
            protocol: ProtocolId::Telegram,
            file_id,
            path,
        },
        None => AdapterEvent::VoiceFailed {
            protocol: ProtocolId::Telegram,
            file_id,
        },
    }
}

async fn finish_photo(client_id: i32, file: tdlib_rs::types::File, secret: bool, events: &EventTx) {
    let file_id = file.id;
    if secret {
        let path = file.local.path;
        let read = tokio::task::spawn_blocking({
            let path = path.clone();
            move || std::fs::read(&path).ok().filter(|bytes| !bytes.is_empty())
        })
        .await;
        // TDLib wrote the file. Drop it after the read. A failed delete still
        // removes the path, so a second process does not keep the bytes.
        if let Err(error) = tdlib_rs::functions::delete_file(file_id, client_id).await {
            log_tdlib_error("deleteFile", &error);
        }
        if !path.is_empty() {
            let _ = tokio::task::spawn_blocking(move || std::fs::remove_file(path)).await;
        }
        match read {
            Ok(Some(bytes)) => {
                let Some(file) = crate::PhotoFile::from_bytes(crate::ImageBytes::new(bytes)) else {
                    emit_photo_failed(events, ProtocolId::Telegram, file_id);
                    return;
                };
                emit_photo_ready(events, ProtocolId::Telegram, file_id, file);
            }
            _ => emit_photo_failed(events, ProtocolId::Telegram, file_id),
        }
        return;
    }
    if file.local.is_downloading_completed && !file.local.path.is_empty() {
        emit_photo_ready(
            events,
            ProtocolId::Telegram,
            file_id,
            crate::PhotoFile::from_path(crate::ImagePath::new(file.local.path)),
        );
    } else {
        emit_photo_failed(events, ProtocolId::Telegram, file_id);
    }
}

async fn load_main_chats(client_id: i32, authorized: bool, events: &EventTx) {
    if !authorized {
        return;
    }
    match tdlib_rs::functions::load_chats(
        Some(tdlib_rs::enums::ChatList::Main),
        inbox::MAIN_CHAT_LIMIT,
        client_id,
    )
    .await
    {
        Ok(()) => emit_status(
            events,
            ProtocolId::Telegram,
            AdapterStatus::Ready,
            "Telegram chat list loaded.",
        ),
        Err(error) if inbox::is_end_of_chat_list(error.code) => emit_status(
            events,
            ProtocolId::Telegram,
            AdapterStatus::Ready,
            "Telegram chat list is up to date.",
        ),
        Err(error) => {
            log_tdlib_error("loadChats", &error);
            emit_status(
                events,
                ProtocolId::Telegram,
                AdapterStatus::Error,
                format!("Could not load Telegram chats (TDLib {}).", error.code),
            );
        }
    }
    emit_chat_list_loaded(events, ProtocolId::Telegram);
}

async fn open_chat(client_id: i32, conversation_id: &str, live: &mut LiveInbox, events: &EventTx) {
    if !live.linked() {
        emit_status(
            events,
            ProtocolId::Telegram,
            AdapterStatus::Error,
            "Telegram is not ready. Open a chat after authorization.",
        );
        return;
    }
    let Some(chat_id) = inbox::parse_telegram_chat_id(conversation_id) else {
        emit_status(
            events,
            ProtocolId::Telegram,
            AdapterStatus::Error,
            "That chat id is not a Telegram chat.",
        );
        return;
    };
    // `ViewChat` owns `live.open_chat`. Loading history is not a view:
    // the user may already have left this thread. `load_history` reads the
    // shared cell, which `view_chat` updates before this arm can finish.
    emit_status(
        events,
        ProtocolId::Telegram,
        AdapterStatus::Ready,
        "Loading recent messages.",
    );
    if live.directory.has_topics(chat_id) {
        if load_forum_topics(client_id, chat_id, live, events).await {
            live.topic_lists.loaded(chat_id);
        } else {
            live.topic_lists.failed(chat_id, Instant::now());
        }
    }
    load_history(client_id, chat_id, live, events).await;
    emit_history_loaded(events, ProtocolId::Telegram, conversation_id);
}

/// Forum topics of one chat, following `total_count` and the next offset.
/// Stops at [`inbox::TOPIC_PAGE_CAP`]. Runs on the TDLib worker.
/// `true` when the list finished. `false` when a page failed, so a later
/// pass can try again.
async fn load_forum_topics(
    client_id: i32,
    chat_id: i64,
    live: &mut LiveInbox,
    events: &EventTx,
) -> bool {
    let mut cursor = inbox::TopicListCursor::start();
    loop {
        let page = tdlib_rs::functions::get_forum_topics(
            chat_id,
            String::new(),
            cursor.offset_date,
            cursor.offset_message_id,
            cursor.offset_forum_topic_id,
            inbox::TOPIC_PAGE_LIMIT,
            client_id,
        )
        .await;
        let Ok(tdlib_rs::enums::ForumTopics::ForumTopics(page)) = page else {
            if let Err(error) = page {
                log_tdlib_error("getForumTopics", &error);
            }
            return false;
        };
        let count = i32::try_from(page.topics.len()).unwrap_or(i32::MAX);
        for topic in page.topics {
            live.topic_lists.note_read(
                chat_id,
                topic.info.forum_topic_id,
                topic.last_read_inbox_message_id,
            );
            let row = forum_topic_row(chat_id, &topic);
            if inbox::cache_topic(&mut live.topics, &row) {
                emit_topic(events, row);
            }
        }
        let Some(next) = cursor.advance(
            count,
            page.total_count,
            page.next_offset_date,
            page.next_offset_message_id,
            page.next_offset_forum_topic_id,
        ) else {
            return true;
        };
        cursor = next;
    }
}

/// Load topic lists for main-list forums that do not have one yet.
/// The worker does this with the chat list, off the UI thread.
async fn load_due_forum_topics(client_id: i32, live: &mut LiveInbox, events: &EventTx) {
    if !live.linked() {
        return;
    }
    let listed = live.directory.listed_topic_chats();
    // A failed list waits before the next try: this pass runs after each update.
    for chat_id in live.topic_lists.due(&listed, Instant::now()) {
        if load_forum_topics(client_id, chat_id, live, events).await {
            live.topic_lists.loaded(chat_id);
        } else {
            live.topic_lists.failed(chat_id, Instant::now());
        }
    }
}

/// One topic, including one that was not in `live.topics`. Unread and the
/// last message come from the fetched row.
///
/// A hidden General topic comes back the same way. `getForumTopic` fills
/// `is_hidden` on the row and does not fail because the topic is hidden,
/// so the core can still apply that topic's mute. The center pane hides
/// that row; this fetch still emits it.
async fn refresh_forum_topic(
    client_id: i32,
    chat_id: i64,
    forum_topic_id: i32,
    live: &mut LiveInbox,
    events: &EventTx,
) -> bool {
    match tdlib_rs::functions::get_forum_topic(chat_id, forum_topic_id, client_id).await {
        Ok(tdlib_rs::enums::ForumTopic::ForumTopic(topic)) => {
            live.topic_lists.note_read(
                chat_id,
                topic.info.forum_topic_id,
                topic.last_read_inbox_message_id,
            );
            let row = forum_topic_row(chat_id, &topic);
            if inbox::cache_topic(&mut live.topics, &row) {
                emit_topic(events, row);
            }
            true
        }
        Err(error) => {
            log_tdlib_error("getForumTopic", &error);
            false
        }
    }
}

fn forum_topic_row(chat_id: i64, topic: &tdlib_rs::types::ForumTopic) -> ChatTopic {
    let info = &topic.info;
    ChatTopic {
        protocol: ProtocolId::Telegram,
        conversation_id: inbox::conversation_id(chat_id),
        topic: MessageTopic::Forum(i64::from(info.forum_topic_id)),
        name: info.name.clone(),
        unread: u32::try_from(topic.unread_count).unwrap_or(0),
        last_at: topic
            .last_message
            .as_ref()
            .map(|message| i64::from(message.date))
            .unwrap_or(0),
        general: info.is_general,
        hidden: info.is_hidden,
        closed: info.is_closed,
        muted: topic.notification_settings.mute_for > 0,
    }
}

async fn open_topic(
    client_id: i32,
    conversation_id: &str,
    topic: MessageTopic,
    live: &mut LiveInbox,
    events: &EventTx,
) {
    if !live.linked() {
        emit_status(
            events,
            ProtocolId::Telegram,
            AdapterStatus::Error,
            "Telegram is not ready. Open a topic after authorization.",
        );
        return;
    }
    let Some(chat_id) = inbox::parse_telegram_chat_id(conversation_id) else {
        emit_status(
            events,
            ProtocolId::Telegram,
            AdapterStatus::Error,
            "That chat id is not a Telegram chat.",
        );
        return;
    };
    emit_status(
        events,
        ProtocolId::Telegram,
        AdapterStatus::Ready,
        "Loading recent messages.",
    );
    load_topic_history(client_id, chat_id, topic, 0, live, events).await;
    emit_history_loaded(events, ProtocolId::Telegram, conversation_id);
}

async fn load_older_topic(
    client_id: i32,
    conversation_id: &str,
    topic: MessageTopic,
    before_message_id: &str,
    live: &mut LiveInbox,
    events: &EventTx,
) {
    let end = |more: bool, note: Option<String>| {
        emit_older_history_loaded(
            events,
            ProtocolId::Telegram,
            conversation_id,
            before_message_id,
            more,
            note,
            Some(topic),
        );
    };
    let Some((chat_id, before)) = inbox::parse_message_id(before_message_id) else {
        end(false, None);
        return;
    };
    if !live.linked() {
        end(true, None);
        return;
    }
    let loaded = load_topic_history(client_id, chat_id, topic, before, live, events).await;
    end(loaded, None);
}

/// Messages of one topic, newest page when `from_message_id` is 0.
/// Returns whether the page was non-empty.
async fn load_topic_history(
    client_id: i32,
    chat_id: i64,
    topic: MessageTopic,
    from_message_id: i64,
    live: &LiveInbox,
    events: &EventTx,
) -> bool {
    let page = match topic {
        MessageTopic::Forum(id) => {
            let Some(forum_topic_id) = i32::try_from(id).ok() else {
                return false;
            };
            tdlib_rs::functions::get_forum_topic_history(
                chat_id,
                forum_topic_id,
                from_message_id,
                0,
                inbox::HISTORY_LIMIT,
                client_id,
            )
            .await
        }
        MessageTopic::Thread(message_id) => {
            tdlib_rs::functions::get_message_thread_history(
                chat_id,
                message_id,
                from_message_id,
                0,
                inbox::HISTORY_LIMIT,
                client_id,
            )
            .await
        }
    };
    match page {
        Ok(tdlib_rs::enums::Messages::Messages(batch)) => {
            let messages: Vec<_> = batch.messages.into_iter().flatten().collect();
            let message_ids: Vec<i64> = messages.iter().map(|message| message.id).collect();
            let any = !messages.is_empty();
            for message in inbox::chronological(messages) {
                emit_mapped_message(
                    client_id,
                    events,
                    &message,
                    live,
                    None,
                    OutgoingUpdate::Listed,
                );
            }
            if !message_ids.is_empty()
                && inbox::should_mark_topic_viewed(
                    live.viewed.get(),
                    live.viewed.get_topic(),
                    chat_id,
                    topic,
                )
                && let Err(error) =
                    tdlib_rs::functions::view_messages(chat_id, message_ids, None, true, client_id)
                        .await
            {
                log_tdlib_error("viewMessages", &error);
            }
            any
        }
        Err(error) => {
            log_tdlib_error("getForumTopicHistory", &error);
            false
        }
    }
}

async fn load_history(client_id: i32, chat_id: i64, live: &mut LiveInbox, events: &EventTx) {
    match tdlib_rs::functions::get_chat_history(
        chat_id,
        0,
        0,
        inbox::HISTORY_LIMIT,
        false,
        client_id,
    )
    .await
    {
        Ok(tdlib_rs::enums::Messages::Messages(batch)) => {
            let messages: Vec<_> = batch.messages.into_iter().flatten().collect();
            let message_ids: Vec<i64> = messages.iter().map(|message| message.id).collect();
            for message in inbox::chronological(messages) {
                emit_mapped_message(
                    client_id,
                    events,
                    &message,
                    live,
                    None,
                    OutgoingUpdate::Listed,
                );
            }
            // Read the cell after `getChatHistory`: `ViewChat` may have been
            // published while this call was in flight, and it is still queued.
            let viewed = live.viewed.get();
            let viewed_topic = live.viewed.get_topic();
            match history_page_read(viewed, viewed_topic, chat_id, &message_ids) {
                HistoryPageRead::Mark { newest } => {
                    if let Err(error) = tdlib_rs::functions::view_messages(
                        chat_id,
                        message_ids,
                        None,
                        true,
                        client_id,
                    )
                    .await
                    {
                        log_tdlib_error("viewMessages", &error);
                        emit_status(
                            events,
                            ProtocolId::Telegram,
                            AdapterStatus::Error,
                            format!("Could not mark messages read (TDLib {}).", error.code),
                        );
                        // The id stays until a later viewMessages succeeds.
                        let mut unviewed =
                            live.unviewed.lock().unwrap_or_else(PoisonError::into_inner);
                        remember_id(&mut unviewed, chat_id, newest);
                        return;
                    }
                }
                HistoryPageRead::Remember(Some(newest)) => {
                    let mut unviewed = live.unviewed.lock().unwrap_or_else(PoisonError::into_inner);
                    remember_id(&mut unviewed, chat_id, newest);
                }
                HistoryPageRead::Remember(None) => {}
            }
            emit_status(
                events,
                ProtocolId::Telegram,
                AdapterStatus::Ready,
                "Recent messages loaded.",
            );
        }
        Err(error) => {
            log_tdlib_error("getChatHistory", &error);
            emit_status(
                events,
                ProtocolId::Telegram,
                AdapterStatus::Error,
                format!("Could not load messages (TDLib {}).", error.code),
            );
        }
    }
}

/// One page of messages older than `before_message_id`. The worker runs
/// one command at a time, so one TDLib history call runs at a time. Message
/// text is never logged.
async fn load_older(
    client_id: i32,
    conversation_id: &str,
    before_message_id: &str,
    live: &mut LiveInbox,
    events: &EventTx,
) {
    let end = |more: bool, note: Option<String>| {
        emit_older_history_loaded(
            events,
            ProtocolId::Telegram,
            conversation_id,
            before_message_id,
            more,
            note,
            None,
        );
    };
    let done = |more: bool| end(more, None);
    let Some((chat_id, before)) = inbox::parse_message_id(before_message_id) else {
        done(false);
        return;
    };
    if !live.linked() {
        done(true);
        return;
    }
    match live.older.begin(chat_id, before) {
        OlderStep::AtStart | OlderStep::Repeat => {
            done(live.older.more(chat_id));
            return;
        }
        OlderStep::Fetch => {}
    }
    // TDLib can answer the first call with only the anchor (a short page
    // before it has older messages). Ask once more in that case.
    let mut page = fetch_older_page(client_id, chat_id, before).await;
    if matches!(page, Ok((PageOutcome::OnlyAnchor, _))) {
        page = fetch_older_page(client_id, chat_id, before).await;
    }
    match page {
        Ok((outcome, older)) => {
            for message in &older {
                emit_mapped_message(
                    client_id,
                    events,
                    message,
                    live,
                    None,
                    OutgoingUpdate::Listed,
                );
            }
            let more = live.older.finish(chat_id, before, outcome);
            let (more, note) = inbox::older_end(Ok(more));
            end(more, note);
        }
        Err(error) => {
            log_tdlib_error("getChatHistory (older)", &error);
            let (more, note) = inbox::older_end(Err(error.code));
            end(more, note);
        }
    }
}

/// One TDLib page older than `before`, classified. Older messages come
/// oldest first, without the anchor.
async fn fetch_older_page(
    client_id: i32,
    chat_id: i64,
    before: i64,
) -> Result<(PageOutcome, Vec<tdlib_rs::types::Message>), tdlib_rs::types::Error> {
    let tdlib_rs::enums::Messages::Messages(batch) = tdlib_rs::functions::get_chat_history(
        chat_id,
        before,
        0,
        inbox::OLDER_PAGE_LIMIT,
        false,
        client_id,
    )
    .await?;
    let messages: Vec<_> = batch.messages.into_iter().flatten().collect();
    let raw_len = messages.len();
    let older = inbox::older_than(messages, before, |message| message.id);
    Ok((inbox::page_outcome(raw_len, older.len()), older))
}

/// One photo or video for TDLib. TDLib reads the local file and finds the
/// size itself. The caption goes on the first file of a message.
fn media_content(
    file: &crate::media::OutgoingFile,
    caption: Option<&str>,
) -> tdlib_rs::enums::InputMessageContent {
    use tdlib_rs::enums::{InputFile, InputMessageContent};
    use tdlib_rs::types::{FormattedText, InputFileLocal, InputMessagePhoto, InputMessageVideo};
    let input = InputFile::Local(InputFileLocal {
        path: file.path.to_string_lossy().into_owned(),
    });
    let caption = caption
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(|text| FormattedText {
            text: text.to_owned(),
            entities: Vec::new(),
        });
    match file.kind {
        crate::media::MediaKind::Photo => {
            InputMessageContent::InputMessagePhoto(InputMessagePhoto {
                photo: input,
                thumbnail: None,
                added_sticker_file_ids: Vec::new(),
                width: 0,
                height: 0,
                caption,
                show_caption_above_media: false,
                self_destruct_type: None,
                has_spoiler: false,
            })
        }
        crate::media::MediaKind::Video => {
            InputMessageContent::InputMessageVideo(InputMessageVideo {
                video: input,
                thumbnail: None,
                cover: None,
                start_timestamp: 0,
                added_sticker_file_ids: Vec::new(),
                duration: 0,
                width: 0,
                height: 0,
                supports_streaming: true,
                caption,
                show_caption_above_media: false,
                self_destruct_type: None,
                has_spoiler: false,
            })
        }
    }
}

/// Send photos and videos: one message for one file, albums of up to
/// [`crate::media::FILES_PER_MESSAGE`] files for more. The caption goes with
/// the first file. The send is accepted when the first message is out. A
/// later album that fails shows its own error.
#[allow(clippy::too_many_arguments)]
async fn send_media(
    client_id: i32,
    conversation_id: &str,
    topic: Option<MessageTopic>,
    files: &[crate::media::OutgoingFile],
    caption: &str,
    request: u64,
    live: &LiveInbox,
    events: &EventTx,
) {
    let rejected = || {
        emit_send_rejected(events, ProtocolId::Telegram, conversation_id, request);
    };
    if !live.linked() || files.is_empty() {
        rejected();
        return;
    }
    let Some(chat_id) = inbox::parse_telegram_chat_id(conversation_id) else {
        rejected();
        return;
    };
    let mut accepted = false;
    for (index, batch) in files.chunks(crate::media::FILES_PER_MESSAGE).enumerate() {
        let contents: Vec<_> = batch
            .iter()
            .enumerate()
            .map(|(at, file)| media_content(file, (index == 0 && at == 0).then_some(caption)))
            .collect();
        let sent: Result<Vec<tdlib_rs::types::Message>, tdlib_rs::types::Error> =
            if let [content] = contents.as_slice() {
                tdlib_rs::functions::send_message(
                    chat_id,
                    topic.and_then(td_topic),
                    None,
                    None,
                    content.clone(),
                    client_id,
                )
                .await
                .map(|tdlib_rs::enums::Message::Message(message)| vec![message])
            } else {
                tdlib_rs::functions::send_message_album(
                    chat_id,
                    topic.and_then(td_topic),
                    None,
                    None,
                    contents,
                    client_id,
                )
                .await
                .map(|tdlib_rs::enums::Messages::Messages(list)| {
                    list.messages.into_iter().flatten().collect()
                })
            };
        match sent {
            Ok(messages) => {
                for message in &messages {
                    emit_mapped_message(
                        client_id,
                        events,
                        message,
                        live,
                        None,
                        OutgoingUpdate::Listed,
                    );
                }
                if !accepted {
                    emit_send_accepted(events, ProtocolId::Telegram, conversation_id, request);
                    accepted = true;
                }
            }
            Err(error) => {
                log_tdlib_error("sendMessageAlbum", &error);
                if !accepted {
                    rejected();
                }
                emit_status(
                    events,
                    ProtocolId::Telegram,
                    AdapterStatus::Error,
                    format!(
                        "Telegram did not send the photos or videos (TDLib {}).",
                        error.code
                    ),
                );
                return;
            }
        }
    }
}

/// Delete one message. TDLib sends `updateDeleteMessages`, and that update
/// removes the row. A refusal names the chat in `CommandFailed`.
async fn delete_message(
    client_id: i32,
    conversation_id: &str,
    message_id: &str,
    for_everyone: bool,
    events: &EventTx,
) {
    let Some((chat_id, id)) = inbox::parse_message_id(message_id) else {
        return;
    };
    if let Err(error) =
        tdlib_rs::functions::delete_messages(chat_id, vec![id], for_everyone, client_id).await
    {
        log_tdlib_error("deleteMessages", &error);
        crate::adapter::emit_command_failed(
            events,
            ProtocolId::Telegram,
            Some(conversation_id.to_string()),
            "Telegram did not delete the message.",
        );
    }
}

/// The new content of one message: its text, links, and media. A text with
/// no entities is scanned for links later.
fn emit_content(
    client_id: i32,
    events: &EventTx,
    chat_id: i64,
    message_id: i64,
    content: &tdlib_rs::enums::MessageContent,
) {
    let mapped = super::media_map::map_content(content);
    let conversation_id = inbox::conversation_id(chat_id);
    let message_id = inbox::message_id(chat_id, message_id);
    emit_message_media(
        events,
        ProtocolId::Telegram,
        conversation_id.clone(),
        message_id.clone(),
        mapped.text.clone(),
        mapped.links,
        mapped.photo,
        mapped.audio,
        mapped.video,
        mapped.file,
    );
    if mapped.scan {
        schedule_text_entities(
            client_id,
            events.clone(),
            conversation_id,
            message_id,
            mapped.text,
        );
    }
}

/// Put `body` in the place of the text of one own message. The reply of
/// `editMessageText` is the edited message, and the row changes from it at
/// once. `updateMessageContent` and `updateMessageEdited` come in the update
/// queue and change the row again with the same text. A message that
/// Telegram does not let this account edit, for example one older than the
/// edit limit, gets `CommandFailed`.
/// Most messages in one `forwardMessages` request (TDLib limit).
const FORWARD_BATCH: usize = 100;

/// The TDLib ids of `message_ids` that are messages of `from_chat`, in a
/// strictly increasing order, as `forwardMessages` needs them.
fn forward_ids(from_chat: i64, message_ids: &[String]) -> Vec<i64> {
    let mut ids: Vec<i64> = message_ids
        .iter()
        .filter_map(|id| inbox::parse_message_id(id))
        .filter(|(chat, _)| *chat == from_chat)
        .map(|(_, id)| id)
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// The notice after a forward. `None`: nothing was forwarded.
fn forward_notice(forwarded: usize, total: usize) -> Option<String> {
    match forwarded {
        0 => None,
        1 if total == 1 => Some("Message forwarded.".into()),
        n if n == total => Some(format!("{n} messages forwarded.")),
        n => Some(format!(
            "{n} of {total} messages forwarded. Telegram does not let you forward the others."
        )),
    }
}

/// Forward messages of one chat to another chat, with their media. TDLib
/// returns null for a message that it cannot forward (for example, from a
/// chat with protected content). The new messages come as updates.
async fn forward_messages(
    client_id: i32,
    from_conversation_id: &str,
    message_ids: &[String],
    to_conversation_id: &str,
    events: &EventTx,
) {
    let failed = |detail: &str| {
        crate::adapter::emit_command_failed(
            events,
            ProtocolId::Telegram,
            Some(from_conversation_id.to_string()),
            detail,
        );
    };
    let (Some(from_chat), Some(to_chat)) = (
        inbox::parse_telegram_chat_id(from_conversation_id),
        inbox::parse_telegram_chat_id(to_conversation_id),
    ) else {
        failed("Telegram did not forward the messages.");
        return;
    };
    let ids = forward_ids(from_chat, message_ids);
    if ids.is_empty() {
        failed("Select a message to forward.");
        return;
    }
    let mut forwarded = 0;
    for batch in ids.chunks(FORWARD_BATCH) {
        match tdlib_rs::functions::forward_messages(
            to_chat,
            None,
            from_chat,
            batch.to_vec(),
            None,
            false,
            false,
            client_id,
        )
        .await
        {
            Ok(tdlib_rs::enums::Messages::Messages(result)) => {
                forwarded += result.messages.iter().flatten().count();
            }
            Err(error) => log_tdlib_error("forwardMessages", &error),
        }
    }
    match forward_notice(forwarded, ids.len()) {
        Some(text) => crate::adapter::emit_notice(events, ProtocolId::Telegram, text),
        None => failed("Telegram did not forward the messages."),
    }
}

async fn edit_message(
    client_id: i32,
    conversation_id: &str,
    message_id: &str,
    body: &str,
    emit: bool,
    events: &EventTx,
) {
    let Some((chat_id, id)) = inbox::parse_message_id(message_id) else {
        return;
    };
    let failed = |detail: &str| {
        crate::adapter::emit_command_failed(
            events,
            ProtocolId::Telegram,
            Some(conversation_id.to_string()),
            detail,
        );
    };
    let text = body.trim();
    if text.is_empty() {
        failed("Telegram does not save an empty message.");
        return;
    }
    match tdlib_rs::functions::get_message_properties(chat_id, id, client_id).await {
        Ok(tdlib_rs::enums::MessageProperties::MessageProperties(properties))
            if properties.can_be_edited => {}
        Ok(_) => {
            failed("Telegram does not let you edit this message.");
            return;
        }
        Err(error) => {
            log_tdlib_error("getMessageProperties", &error);
            failed("Telegram did not save the edit.");
            return;
        }
    }
    let content =
        tdlib_rs::enums::InputMessageContent::InputMessageText(tdlib_rs::types::InputMessageText {
            text: tdlib_rs::types::FormattedText {
                text: text.to_string(),
                entities: Vec::new(),
            },
            link_preview_options: None,
            clear_draft: false,
        });
    match tdlib_rs::functions::edit_message_text(chat_id, id, content, client_id).await {
        Ok(tdlib_rs::enums::Message::Message(edited)) => {
            if emit {
                emit_content(client_id, events, chat_id, edited.id, &edited.content);
                emit_message_edited(
                    events,
                    ProtocolId::Telegram,
                    inbox::conversation_id(chat_id),
                    inbox::message_id(chat_id, edited.id),
                );
            }
        }
        Err(error) => {
            log_tdlib_error("editMessageText", &error);
            failed("Telegram did not save the edit.");
        }
    }
}

/// Add or remove an emoji reaction. TDLib sends the new counts in
/// `updateMessageInteractionInfo`.
async fn react(
    client_id: i32,
    conversation_id: &str,
    message_id: &str,
    emoji: &str,
    add: bool,
    events: &EventTx,
) {
    let Some((chat_id, id)) = inbox::parse_message_id(message_id) else {
        return;
    };
    let reaction = tdlib_rs::enums::ReactionType::Emoji(tdlib_rs::types::ReactionTypeEmoji {
        emoji: emoji.to_string(),
    });
    let result = if add {
        tdlib_rs::functions::add_message_reaction(chat_id, id, reaction, false, true, client_id)
            .await
    } else {
        tdlib_rs::functions::remove_message_reaction(chat_id, id, reaction, client_id).await
    };
    if let Err(error) = result {
        log_tdlib_error("messageReaction", &error);
        crate::adapter::emit_command_failed(
            events,
            ProtocolId::Telegram,
            Some(conversation_id.to_string()),
            "Telegram did not take the reaction.",
        );
    }
}

#[allow(clippy::too_many_arguments)]
async fn send_text(
    client_id: i32,
    conversation_id: &str,
    body: &str,
    request: u64,
    topic: Option<MessageTopic>,
    reply_to: Option<&str>,
    live: &LiveInbox,
    events: &EventTx,
) {
    // Every refusal names this send, so the UI fails only this send.
    let rejected = || {
        emit_send_rejected(events, ProtocolId::Telegram, conversation_id, request);
    };
    if !live.linked() {
        rejected();
        emit_status(
            events,
            ProtocolId::Telegram,
            AdapterStatus::Error,
            "Telegram is not ready. Send after authorization.",
        );
        return;
    }
    let Some(chat_id) = inbox::parse_telegram_chat_id(conversation_id) else {
        rejected();
        emit_status(
            events,
            ProtocolId::Telegram,
            AdapterStatus::Error,
            "That chat id is not a Telegram chat.",
        );
        return;
    };
    let text = body.trim();
    if text.is_empty() {
        rejected();
        return;
    }
    let content =
        tdlib_rs::enums::InputMessageContent::InputMessageText(tdlib_rs::types::InputMessageText {
            text: tdlib_rs::types::FormattedText {
                text: text.to_string(),
                entities: Vec::new(),
            },
            link_preview_options: None,
            clear_draft: true,
        });
    // A reply in the same chat. A row id of another chat is not a reply.
    let reply = reply_to
        .and_then(inbox::parse_message_id)
        .filter(|(reply_chat, _)| *reply_chat == chat_id)
        .map(|(_, message_id)| {
            tdlib_rs::enums::InputMessageReplyTo::Message(
                tdlib_rs::types::InputMessageReplyToMessage {
                    message_id,
                    quote: None,
                    checklist_task_id: 0,
                },
            )
        });
    match tdlib_rs::functions::send_message(
        chat_id,
        topic.and_then(td_topic),
        reply,
        None,
        content,
        client_id,
    )
    .await
    {
        Ok(tdlib_rs::enums::Message::Message(message)) => {
            emit_mapped_message(
                client_id,
                events,
                &message,
                live,
                None,
                OutgoingUpdate::Listed,
            );
            // After the pending row, name this send as accepted.
            emit_send_accepted(events, ProtocolId::Telegram, conversation_id, request);
        }
        Err(error) => {
            log_tdlib_error("sendMessage", &error);
            rejected();
            emit_status(
                events,
                ProtocolId::Telegram,
                AdapterStatus::Error,
                format!("Telegram did not send the message (TDLib {}).", error.code),
            );
        }
    }
}

async fn resend(
    client_id: i32,
    conversation_id: &str,
    message_id: &str,
    request: u64,
    live: &LiveInbox,
    events: &EventTx,
) {
    // Every failure names the request, so the shell ends only this retry.
    let failed = || {
        emit_message_delivery(
            events,
            ProtocolId::Telegram,
            conversation_id,
            message_id,
            Delivery::Failed,
        );
        emit_send_rejected(events, ProtocolId::Telegram, conversation_id, request);
    };
    if !live.linked() {
        failed();
        emit_status(
            events,
            ProtocolId::Telegram,
            AdapterStatus::Error,
            "Telegram is not ready. Retry after authorization.",
        );
        return;
    }
    let Some((chat_id, id)) = inbox::parse_message_id(message_id) else {
        failed();
        return;
    };
    // TDLib deletes the failed row and returns a new pending message, or null
    // when the message cannot be sent again.
    match tdlib_rs::functions::resend_messages(chat_id, vec![id], None, 0, client_id).await {
        Ok(tdlib_rs::enums::Messages::Messages(batch)) => {
            match batch.messages.into_iter().flatten().next() {
                Some(message) => {
                    emit_mapped_message(
                        client_id,
                        events,
                        &message,
                        live,
                        Some(message_id.to_string()),
                        OutgoingUpdate::Listed,
                    );
                    emit_send_accepted(events, ProtocolId::Telegram, conversation_id, request);
                }
                None => {
                    failed();
                    emit_status(
                        events,
                        ProtocolId::Telegram,
                        AdapterStatus::Error,
                        "Telegram cannot send this message again.",
                    );
                }
            }
        }
        Err(error) => {
            log_tdlib_error("resendMessages", &error);
            failed();
            emit_status(
                events,
                ProtocolId::Telegram,
                AdapterStatus::Error,
                format!("Telegram did not send the message (TDLib {}).", error.code),
            );
        }
    }
}

async fn set_parameters(
    client_id: i32,
    secrets: &dyn TelegramSecretVault,
    source: &TelegramApiSource,
    events: &EventTx,
    closing: &ClosingFlag,
) {
    let login = LoginEvents::new(events, closing);
    let (api_id, api_hash) = match require_resolved_api(secrets, source) {
        Ok(pair) => pair,
        Err(_) => {
            login.phase(TelegramAuthPhase::Failed);
            emit_status(
                events,
                ProtocolId::Telegram,
                AdapterStatus::Error,
                "telegram api credentials are missing; set a keychain override or rebuild with TELEGRAM_API_ID",
            );
            return;
        }
    };
    let Ok(api_id) = parse_resolved_api_id(&api_id) else {
        login.phase(TelegramAuthPhase::Failed);
        emit_status(
            events,
            ProtocolId::Telegram,
            AdapterStatus::Error,
            "telegram api_id must be a number",
        );
        return;
    };
    // A keychain read error is not a missing key. Refuse to open the folder
    // until a clean hydrate has settled. Only Ok(None) on a settled vault is
    // keyless; an unsettled vault must not reach move-aside or a new key.
    if !secrets.secrets_hydrated() {
        tracing::warn!("telegram secrets are not hydrated; not opening the data folder");
        login.rejected(TelegramAuthError::ClientSetup { code: 0 });
        login.phase(TelegramAuthPhase::Failed);
        emit_status(
            events,
            ProtocolId::Telegram,
            AdapterStatus::Error,
            "Telegram could not start (keychain still opening).",
        );
        return;
    }
    let dir = match tdlib_data_dir(secrets.persists(), &secrets.tdlib_folder_name()) {
        Ok(dir) => dir,
        Err(error) => {
            tracing::warn!(%error, "no private folder for the Telegram session");
            login.rejected(TelegramAuthError::ClientSetup { code: 0 });
            login.phase(TelegramAuthPhase::Failed);
            emit_status(
                events,
                ProtocolId::Telegram,
                AdapterStatus::Error,
                "Telegram could not start (no private data folder).",
            );
            return;
        }
    };
    // A folder whose key the vault lost can never open again. Move it aside
    // before the first try. This checks the vault before a new key is made.
    let has_key = secrets
        .get_secret(TelegramSecretKey::DbEncryption)
        .is_some_and(|key| !key.is_empty());
    let mut moved_to = move_aside(data_dir::move_aside_if_keyless(&dir, has_key));
    ensure_dir(&dir);
    let mut result = send_parameters(
        client_id,
        &dir,
        ensure_db_key(secrets, events),
        api_id,
        &api_hash,
    )
    .await;
    if let Err(error) = &result
        && moved_to.is_none()
        && data_dir::is_wrong_key_error(error.code, &error.message)
    {
        // The key in the vault does not open this folder. Keep the old folder,
        // start a fresh one with a new key, and try once more.
        log_tdlib_error("setTdlibParameters", error);
        moved_to = move_aside(data_dir::move_aside(&dir).map(Some));
        if moved_to.is_some() {
            secrets.set_secret(TelegramSecretKey::DbEncryption, "");
            ensure_dir(&dir);
            result = send_parameters(
                client_id,
                &dir,
                ensure_db_key(secrets, events),
                api_id,
                &api_hash,
            )
            .await;
        }
    }
    // All old folders are kept (never deleted). The notice names the new one once.
    if let Some(name) = &moved_to {
        login.data_reset(name);
    }
    if let Err(error) = result {
        // No login step can run now. Stop the flow; the UI must not show
        // the phone step (TDLib would answer "call setTdlibParameters first").
        log_tdlib_error("setTdlibParameters", &error);
        login.rejected(TelegramAuthError::ClientSetup { code: error.code });
        login.phase(TelegramAuthPhase::Failed);
        emit_status(
            events,
            ProtocolId::Telegram,
            AdapterStatus::Error,
            format!("Telegram could not start (error {}).", error.code),
        );
    }
}

async fn send_parameters(
    client_id: i32,
    dir: &std::path::Path,
    encryption_key: String,
    api_id: i32,
    api_hash: &str,
) -> Result<(), tdlib_rs::types::Error> {
    tdlib_rs::functions::set_tdlib_parameters(
        false,
        dir.display().to_string(),
        String::new(),
        encryption_key,
        true,
        true,
        true,
        false,
        api_id,
        api_hash.to_string(),
        "en".into(),
        "thinwire".into(),
        String::new(),
        env!("CARGO_PKG_VERSION").into(),
        client_id,
    )
    .await
}

/// Log the result of a move-aside. `true` when the folder moved.
/// Log the result of a move-aside. Returns the new folder name when it moved.
fn move_aside(result: std::io::Result<Option<PathBuf>>) -> Option<String> {
    match result {
        Ok(Some(moved)) => {
            let name = moved.file_name().map_or_else(
                || "tdlib.stale".to_string(),
                |name| name.to_string_lossy().into_owned(),
            );
            tracing::warn!(
                moved_to = %name,
                "telegram data folder could not open with the saved key; moved aside"
            );
            Some(name)
        }
        Ok(None) => None,
        Err(error) => {
            tracing::warn!(%error, "telegram data folder could not be moved aside");
            None
        }
    }
}

/// Variant name of a TDLib sending state. `None` is a null state.
/// The failed state's error text stays out of the log.
fn sending_state_name(state: &Option<tdlib_rs::enums::MessageSendingState>) -> &'static str {
    match state {
        None => "None",
        Some(tdlib_rs::enums::MessageSendingState::Pending(_)) => "Pending",
        Some(tdlib_rs::enums::MessageSendingState::Failed(_)) => "Failed",
    }
}

/// Log a TDLib error. The text goes to the log only when it cannot hold a
/// phone number or a code.
fn log_tdlib_error(request: &str, error: &tdlib_rs::types::Error) {
    match super::auth_error::loggable_tdlib_message(&error.message) {
        Some(message) => tracing::warn!(request, code = error.code, message, "tdlib error"),
        None => tracing::warn!(request, code = error.code, "tdlib error (text not logged)"),
    }
}

fn ensure_db_key(vault: &dyn TelegramSecretVault, events: &EventTx) -> String {
    if let Some(existing) = vault.get_secret(TelegramSecretKey::DbEncryption)
        && !existing.is_empty()
    {
        return existing;
    }
    let key = generate_db_key();
    vault.set_secret(TelegramSecretKey::DbEncryption, &key);
    super::request_secret_flush(events);
    key
}

fn generate_db_key() -> String {
    super::db_key::generate_db_key()
}

/// The TDLib folder. `THINWIRE_TDLIB_DIR` wins. Without a keychain that
/// persists, each process uses its own private throwaway folder
/// (see [`data_dir::this_process_session_dir`]).
fn tdlib_data_dir(persists: bool, folder_name: &str) -> std::io::Result<PathBuf> {
    if persists || std::env::var_os("THINWIRE_TDLIB_DIR").is_some() {
        Ok(data_dir::persistent_dir(folder_name))
    } else {
        data_dir::this_process_session_dir()
    }
}

/// Create the folder, readable by this user only.
fn ensure_dir(path: &std::path::Path) {
    let _ = std::fs::create_dir_all(path);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telegram::inbox::{ChatDirectory, ChatEffect, ChatMute, ChatSeed, MuteScope};

    #[test]
    fn forward_ids_are_of_the_source_chat_and_strictly_increasing() {
        let ids = [
            "telegram:7:30".to_string(),
            "telegram:7:10".to_string(),
            "telegram:8:20".to_string(),
            "telegram:7:30".to_string(),
            "not-an-id".to_string(),
        ];
        assert_eq!(forward_ids(7, &ids), vec![10, 30]);
        assert!(forward_ids(9, &ids).is_empty());
        let many: Vec<String> = (1..=250).map(|id| format!("telegram:7:{id}")).collect();
        let batches: Vec<usize> = forward_ids(7, &many)
            .chunks(FORWARD_BATCH)
            .map(<[i64]>::len)
            .collect();
        assert_eq!(batches, vec![100, 100, 50], "TDLib takes 100 at most");
    }

    #[test]
    fn the_forward_notice_counts_what_telegram_forwarded() {
        assert_eq!(forward_notice(0, 3), None);
        assert_eq!(forward_notice(1, 1).as_deref(), Some("Message forwarded."));
        assert_eq!(
            forward_notice(3, 3).as_deref(),
            Some("3 messages forwarded.")
        );
        let partial = forward_notice(2, 3).expect("notice");
        assert!(
            partial.starts_with("2 of 3 messages forwarded."),
            "{partial}"
        );
    }

    /// A stand-in for the TDLib client. It records `viewMessages` and answers
    /// with unread 0, which is the `updateChatReadInbox` that follows a view.
    struct FakeTelegram {
        open_chat: Option<i64>,
        viewed: Vec<(i64, i64)>,
        unviewed: HashMap<i64, i64>,
    }

    impl FakeTelegram {
        fn new() -> Self {
            Self {
                open_chat: None,
                viewed: Vec::new(),
                unviewed: HashMap::new(),
            }
        }

        /// `Some(0)` when focus marks messages that arrived while the chat
        /// was not viewed. That is the unread upsert after `viewMessages`.
        fn view(&mut self, conversation_id: Option<&str>) -> Option<i32> {
            self.finish_catch_up(conversation_id, true)
        }

        /// Focus catch-up. `succeed` is false when `viewMessages` fails:
        /// the id stays for the next focus (#174 item 13).
        fn finish_catch_up(&mut self, conversation_id: Option<&str>, succeed: bool) -> Option<i32> {
            apply_viewed_chat(&mut self.open_chat, conversation_id);
            let (chat_id, message_id) = catch_up_read(self.open_chat, &self.unviewed)?;
            if !succeed {
                return None;
            }
            self.view_messages(chat_id, message_id);
            note_view_succeeded(&mut self.unviewed, chat_id, message_id);
            Some(0)
        }

        /// A history page returned. The viewed cell is what the mark reads.
        /// A blur stores the newest id for focus (#174 item 12).
        /// `mark_succeeds` is false when `viewMessages` for the open chat fails.
        fn load_history_page(
            &mut self,
            chat_id: i64,
            message_ids: &[i64],
            viewed: Option<i64>,
            topic: Option<MessageTopic>,
            mark_succeeds: bool,
        ) {
            match history_page_read(viewed, topic, chat_id, message_ids) {
                HistoryPageRead::Mark { newest } => {
                    if mark_succeeds {
                        self.view_messages(chat_id, newest);
                    } else {
                        remember_id(&mut self.unviewed, chat_id, newest);
                    }
                }
                HistoryPageRead::Remember(Some(newest)) => {
                    remember_id(&mut self.unviewed, chat_id, newest);
                }
                HistoryPageRead::Remember(None) => {}
            }
        }

        fn view_messages(&mut self, chat_id: i64, message_id: i64) {
            self.viewed.push((chat_id, message_id));
        }

        /// `Some(0)` when this live message is marked viewed on the server.
        fn receive_live(&mut self, chat_id: i64, message_id: i64, outgoing: bool) -> Option<i32> {
            if outgoing {
                return None;
            }
            if let Some((chat_id, message_id)) =
                inbox::live_message_to_view(self.open_chat, chat_id, message_id)
            {
                self.view_messages(chat_id, message_id);
                return Some(0);
            }
            remember_unviewed(
                &live_message(chat_id, message_id, false),
                self.open_chat,
                &mut self.unviewed,
            );
            None
        }
    }

    #[test]
    fn a_live_message_in_the_open_chat_is_marked_viewed() {
        let mut fake = FakeTelegram::new();
        fake.view(Some("telegram:42"));
        let mut directory = ChatDirectory::new();
        directory.upsert(
            42,
            ChatSeed {
                title: "Ada",
                order: 1,
                unread: 3,
                preview: "earlier",
                participant: "Ada",
                last_at: 1,
                is_group: false,
                scope: MuteScope::Private,
                mute: ChatMute::default(),
                has_topics: false,
                peer: None,
                photo_file_id: None,
                accent: 0,
                last_read_outbox: 0,
            },
        );
        assert!(
            fake.receive_live(7, 1, false).is_none(),
            "a closed chat is not marked"
        );
        let next = fake
            .receive_live(42, 99, false)
            .expect("the open chat is marked viewed");
        assert_eq!(fake.viewed, vec![(42, 99)]);
        assert!(fake.receive_live(42, 100, true).is_none(), "own message");
        let Some(ChatEffect::Upsert(row)) = directory.set_unread(42, next) else {
            panic!("unread upsert");
        };
        assert_eq!(row.unread, 0);

        // Blur sends `ViewChat(None)`. A live message after that is not marked.
        fake.view(None);
        assert!(
            fake.receive_live(42, 101, false).is_none(),
            "after ViewChat(None), a live message is not passed to viewMessages"
        );
        assert!(
            !fake.viewed.contains(&(42, 101)),
            "blur does not call viewMessages"
        );
        fake.view(Some("not-a-telegram-chat"));
        assert!(
            fake.receive_live(42, 102, false).is_none(),
            "a foreign id does not keep the old chat open"
        );
        fake.view(Some("telegram:42"));
        assert_eq!(
            fake.receive_live(42, 103, false),
            Some(0),
            "focus names the chat again"
        );
    }

    #[test]
    fn a_blurred_live_message_is_marked_viewed_when_focus_returns() {
        let mut fake = FakeTelegram::new();
        let mut directory = ChatDirectory::new();
        directory.upsert(
            42,
            ChatSeed {
                title: "Ada",
                order: 1,
                unread: 1,
                preview: "earlier",
                participant: "Ada",
                last_at: 1,
                is_group: false,
                scope: MuteScope::Private,
                mute: ChatMute::default(),
                has_topics: false,
                peer: None,
                photo_file_id: None,
                accent: 0,
                last_read_outbox: 0,
            },
        );
        fake.view(Some("telegram:42"));
        fake.view(None);
        assert!(
            fake.receive_live(42, 101, false).is_none(),
            "a message that arrives while blurred is not marked yet"
        );
        assert!(
            !fake.viewed.contains(&(42, 101)),
            "blur does not call viewMessages"
        );
        let unread = fake
            .view(Some("telegram:42"))
            .expect("focus marks through the message that arrived while blurred");
        assert_eq!(fake.viewed, vec![(42, 101)]);
        let Some(ChatEffect::Upsert(row)) = directory.set_unread(42, unread) else {
            panic!("unread upsert");
        };
        assert_eq!(row.unread, 0);
    }

    /// #174 item 12: a history page that returns after the window blurred
    /// keeps its newest id. Focus calls `viewMessages` for that id.
    #[test]
    fn a_blurred_history_load_is_marked_viewed_when_focus_returns() {
        let mut open = FakeTelegram::new();
        let viewed = inbox::ViewedChat::new();
        let _opened = viewed.publish(Some(42));
        open.load_history_page(42, &[10, 40, 25], viewed.get(), viewed.get_topic(), true);
        assert_eq!(
            open.viewed,
            vec![(42, 40)],
            "a page of the open chat is marked now"
        );
        assert!(
            open.view(Some("telegram:42")).is_none(),
            "that page is not waiting for focus"
        );

        let mut blurred = FakeTelegram::new();
        let _blurred = viewed.publish(None);
        blurred.load_history_page(42, &[10, 40, 25], viewed.get(), viewed.get_topic(), true);
        assert!(
            blurred.viewed.is_empty(),
            "a blurred history load does not call viewMessages"
        );
        let unread = blurred
            .view(Some("telegram:42"))
            .expect("focus marks the newest skipped history id");
        assert_eq!(blurred.viewed, vec![(42, 40)]);
        assert_eq!(unread, 0);

        // A topic of this chat is open. The chat-wide page must not be stored
        // for a later chat-wide catch-up.
        let mut topic = FakeTelegram::new();
        let _topic = viewed.publish_topic(Some(42), Some(MessageTopic::Forum(3)));
        topic.load_history_page(42, &[1, 8], viewed.get(), viewed.get_topic(), true);
        assert!(topic.viewed.is_empty());
        assert!(
            topic.view(Some("telegram:42")).is_none(),
            "a topic of this chat does not store the chat-wide ids"
        );
    }

    /// A history page of the open chat whose `viewMessages` fails keeps the
    /// newest id. The next focus calls `viewMessages` again.
    #[test]
    fn a_failed_open_history_view_keeps_the_newest_id() {
        let mut fake = FakeTelegram::new();
        let viewed = inbox::ViewedChat::new();
        let _opened = viewed.publish(Some(42));
        fake.load_history_page(42, &[10, 40, 25], viewed.get(), viewed.get_topic(), false);
        assert!(
            fake.viewed.is_empty(),
            "a failed viewMessages is not a read"
        );
        let unread = fake
            .view(Some("telegram:42"))
            .expect("focus marks the newest id from the failed page");
        assert_eq!(fake.viewed, vec![(42, 40)]);
        assert_eq!(unread, 0);
        assert!(
            fake.view(Some("telegram:42")).is_none(),
            "success drops the id"
        );
    }

    /// #174 item 13: a failed `viewMessages` keeps the id. The next focus
    /// calls it again. Success drops the id. A newer id that arrived while
    /// the failed call was in flight stays.
    #[test]
    fn a_failed_view_messages_keeps_the_id_until_it_succeeds() {
        let mut fake = FakeTelegram::new();
        fake.view(Some("telegram:42"));
        fake.view(None);
        assert!(fake.receive_live(42, 101, false).is_none());
        assert!(
            fake.finish_catch_up(Some("telegram:42"), false).is_none(),
            "a failed viewMessages is not a read"
        );
        assert!(fake.viewed.is_empty());
        let unread = fake
            .view(Some("telegram:42"))
            .expect("the next focus calls viewMessages again");
        assert_eq!(fake.viewed, vec![(42, 101)]);
        assert_eq!(unread, 0);
        assert!(
            fake.view(Some("telegram:42")).is_none(),
            "success drops the id"
        );

        let mut unviewed = HashMap::from([(42, 101)]);
        remember_id(&mut unviewed, 42, 110);
        note_view_succeeded(&mut unviewed, 42, 101);
        assert_eq!(
            unviewed.get(&42),
            Some(&110),
            "a late success of the old id keeps a newer one"
        );
    }

    #[test]
    fn a_view_request_is_applied_when_the_worker_becomes_ready() {
        let mut runtime = TdlibRuntime::new();
        runtime.view_chat(Some("telegram:42".into()));
        assert!(
            runtime.commands.is_none(),
            "view_chat must not start a client"
        );
        let cell = inbox::ViewedChat::new();
        let mut open = None;
        apply_requested_view(&runtime.requested_view, &cell, &mut open);
        assert_eq!(open, Some(42));
        assert_eq!(cell.get(), Some(42));

        // The worker died. A new cell starts empty. The request is still kept.
        let restarted = inbox::ViewedChat::new();
        open = None;
        apply_requested_view(&runtime.requested_view, &restarted, &mut open);
        assert_eq!(open, Some(42));
        assert_eq!(restarted.get(), Some(42));

        runtime.view_chat(None);
        assert!(runtime.commands.is_none());
        apply_requested_view(&runtime.requested_view, &restarted, &mut open);
        assert_eq!(open, None);
        assert_eq!(restarted.get(), None);
    }

    #[test]
    fn a_ready_restore_loses_to_a_newer_blur() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        let mut runtime = TdlibRuntime::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        runtime.commands = Some(tx);
        for _ in 0..32 {
            runtime.view_chat(Some("telegram:42".into()));
            while rx.try_recv().is_ok() {}
            let requested = Arc::clone(&runtime.requested_view);
            let viewed = runtime.viewed.clone();
            let start = Arc::new(Barrier::new(2));
            let ready = thread::spawn({
                let start = Arc::clone(&start);
                move || {
                    start.wait();
                    let mut open = Some(42);
                    apply_requested_view(&requested, &viewed, &mut open);
                    open
                }
            });
            start.wait();
            runtime.view_chat(None);
            let mut open = ready.join().expect("ready");
            let mut topic = None;
            while let Ok(TdlibCommand::ViewChat {
                conversation_id,
                stamp,
            }) = rx.try_recv()
            {
                apply_view_command(
                    &runtime.viewed,
                    &mut open,
                    &mut topic,
                    conversation_id.as_deref(),
                    stamp,
                );
            }
            assert_eq!(runtime.viewed.get(), None, "the blur stays published");
            assert_eq!(open, None, "the worker does not keep the old chat");
        }
    }

    fn live_message(chat_id: i64, message_id: i64, outgoing: bool) -> tdlib_rs::enums::Update {
        use tdlib_rs::enums::{MessageContent, MessageSender, Update};
        use tdlib_rs::types::{
            FormattedText, Message, MessageSenderUser, MessageText, UpdateNewMessage,
        };
        Update::NewMessage(UpdateNewMessage {
            message: Message {
                id: message_id,
                sender_id: MessageSender::User(MessageSenderUser { user_id: 1 }),
                chat_id,
                sending_state: None,
                scheduling_state: None,
                is_outgoing: outgoing,
                is_pinned: false,
                is_from_offline: false,
                can_be_saved: true,
                has_timestamped_media: false,
                is_channel_post: false,
                is_paid_star_suggested_post: false,
                is_paid_ton_suggested_post: false,
                contains_unread_mention: false,
                date: 1,
                edit_date: 0,
                forward_info: None,
                import_info: None,
                interaction_info: None,
                unread_reactions: Vec::new(),
                fact_check: None,
                suggested_post_info: None,
                reply_to: None,
                topic_id: None,
                self_destruct_type: None,
                self_destruct_in: 0.0,
                auto_delete_in: 0.0,
                via_bot_user_id: 0,
                sender_business_bot_user_id: 0,
                sender_boost_count: 0,
                paid_message_star_count: 0,
                author_signature: String::new(),
                media_album_id: 0,
                effect_id: 0,
                restriction_info: None,
                summary_language_code: String::new(),
                content: MessageContent::MessageText(MessageText {
                    text: FormattedText {
                        text: "hi".into(),
                        entities: Vec::new(),
                    },
                    link_preview: None,
                    link_preview_options: None,
                }),
                reply_markup: None,
            },
        })
    }

    #[test]
    fn a_message_after_closing_starts_is_not_marked_viewed() {
        let mut runtime = TdlibRuntime::new();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        runtime.commands = Some(tx);
        let closing = ClosingFlag::new(Arc::clone(&runtime.login_epoch));
        runtime.current_closing = Some(closing.clone());
        let _ = runtime.viewed.publish(Some(42));
        let mut live = LiveInbox {
            authorized: true,
            older: OlderHistory::new(),
            new_login: false,
            late_ready: false,
            close_cancel: None,
            closing: closing.clone(),
            close_requested: false,
            ended_elsewhere: false,
            directory: ChatDirectory::new(),
            names: NameBook::new(),
            open_chat: Some(42),
            open_topic: None,
            topics: HashMap::new(),
            topic_lists: inbox::TopicLists::default(),
            viewed: runtime.viewed.clone(),
            unviewed: Arc::new(Mutex::new(HashMap::new())),
            downloads: HashMap::new(),
        };
        // Disconnect or Shutdown has marked closing. The cell can still name
        // the chat until `stop_client` clears it.
        live.closing.mark();
        let update = live_message(42, 200, false);
        assert!(
            live_read_while_linked(&live, &update).is_none(),
            "closing started, then a NewMessage: no viewMessages"
        );

        runtime.stop();
        assert!(closing.is_set());
        assert_eq!(runtime.viewed.get(), None, "closing clears the viewed chat");
        live.open_chat = live.viewed.get();
        assert!(live_read_while_linked(&live, &update).is_none());
    }

    #[test]
    fn live_read_of_marks_an_incoming_message_in_the_open_chat() {
        let incoming = live_message(42, 99, false);
        assert_eq!(live_read_of(&incoming, Some(42)), Some((42, 99)));
        assert_eq!(live_read_of(&incoming, Some(7)), None);
        assert_eq!(live_read_of(&incoming, None), None);
        let outgoing = live_message(42, 100, true);
        assert_eq!(live_read_of(&outgoing, Some(42)), None);
    }

    #[test]
    fn an_empty_voice_path_is_a_failed_download() {
        let failed = voice_event(42, true, "");
        assert!(matches!(
            failed,
            AdapterEvent::VoiceFailed {
                protocol: ProtocolId::Telegram,
                file_id: 42,
            }
        ));
        let waiting = voice_event(42, false, "/tmp/hidden-voice.ogg");
        assert!(matches!(
            waiting,
            AdapterEvent::VoiceFailed { file_id: 42, .. }
        ));
        let ready = voice_event(42, true, "/tmp/hidden-voice.ogg");
        match ready {
            AdapterEvent::VoiceReady { file_id, path, .. } => {
                assert_eq!(file_id, 42);
                assert_eq!(
                    path.as_path(),
                    std::path::Path::new("/tmp/hidden-voice.ogg")
                );
                assert!(!format!("{path:?}").contains("hidden-voice"));
            }
            other => panic!("ready voice file, got {other:?}"),
        }
    }

    fn chat_action(
        sender: tdlib_rs::enums::MessageSender,
        topic: Option<tdlib_rs::enums::MessageTopic>,
        action: tdlib_rs::enums::ChatAction,
    ) -> tdlib_rs::types::UpdateChatAction {
        tdlib_rs::types::UpdateChatAction {
            chat_id: 42,
            topic_id: topic,
            sender_id: sender,
            action,
        }
    }

    /// Only "typing a message" is typing. Cancel and every other action end
    /// it. A user takes the name from the name book, a chat its title.
    #[test]
    fn a_chat_action_is_a_typing_event_of_its_sender() {
        use tdlib_rs::enums::{ChatAction, MessageSender, MessageTopic as TdTopic};
        use tdlib_rs::types::{MessageSenderChat, MessageSenderUser, MessageTopicForum};
        let mut names = NameBook::new();
        names.remember_user(7, "Ada", "Lovelace");
        let directory = ChatDirectory::new();
        let user = MessageSender::User(MessageSenderUser { user_id: 7 });
        let forum = TdTopic::Forum(MessageTopicForum { forum_topic_id: 3 });

        let event = typing_event(
            &chat_action(user.clone(), Some(forum), ChatAction::Typing),
            &names,
            &directory,
        );
        assert_eq!(
            event,
            AdapterEvent::Typing {
                protocol: ProtocolId::Telegram,
                conversation_id: "telegram:42".into(),
                topic: Some(MessageTopic::Forum(3)),
                sender_id: "user:7".into(),
                sender: "Ada Lovelace".into(),
                typing: true,
            }
        );
        for action in [ChatAction::Cancel, ChatAction::RecordingVoiceNote] {
            let event = typing_event(&chat_action(user.clone(), None, action), &names, &directory);
            assert!(
                matches!(
                    event,
                    AdapterEvent::Typing {
                        typing: false,
                        topic: None,
                        ..
                    }
                ),
                "{event:?}"
            );
        }
        let chat = MessageSender::Chat(MessageSenderChat { chat_id: 9 });
        let event = typing_event(
            &chat_action(chat, None, ChatAction::Typing),
            &names,
            &directory,
        );
        assert!(
            matches!(
                &event,
                AdapterEvent::Typing { sender_id, sender, typing: true, .. }
                    if sender_id == "chat:9" && sender == "Telegram chat"
            ),
            "{event:?}"
        );
    }

    /// A typing state goes to the chat, or to its topic. An id that is not a
    /// Telegram chat goes nowhere.
    #[test]
    fn a_typing_state_names_its_chat_and_topic() {
        assert!(matches!(
            typing_target("telegram:42", None),
            Some((42, None))
        ));
        assert!(matches!(
            typing_target("telegram:42", Some(MessageTopic::Forum(3))),
            Some((
                42,
                Some(tdlib_rs::enums::MessageTopic::Forum(
                    tdlib_rs::types::MessageTopicForum { forum_topic_id: 3 }
                ))
            ))
        ));
        assert!(typing_target("discord:42", None).is_none());
        let too_big = MessageTopic::Forum(i64::from(i32::MAX) + 1);
        assert!(
            typing_target("telegram:42", Some(too_big)).is_none(),
            "never the whole chat in place of the topic"
        );
    }
}
