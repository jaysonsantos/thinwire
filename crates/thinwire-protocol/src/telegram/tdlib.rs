//! Live `tdlib-rs` client. Compiled only with `--features telegram-tdlib`.
//!
//! FFI and network I/O stay on a dedicated receive thread plus tokio tasks.
//! Credential values are read from the vault and never logged.

use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use super::credentials::{TelegramApiSource, parse_resolved_api_id, require_resolved_api};
use super::data_dir;
use super::inbox::{
    self, ChatDirectory, ChatEffect, ChatSeed, InboxMessage, MessageParty, NameBook,
};
use super::lifecycle::{DoneFlag, WorkerSlots, all_done, mark_done};
use super::router::Router;
use crate::adapter::{
    AdapterStatus, Delivery, EventTx, ProtocolId, TelegramAuthError, TelegramAuthPhase,
    TelegramAuthStep, TelegramCodeVia, emit_chat_list_loaded, emit_conversation,
    emit_conversation_removed, emit_history_loaded, emit_message, emit_message_body,
    emit_message_delivery, emit_message_replaced, emit_messages_removed, emit_status, emit_stopped,
    emit_telegram_auth, emit_telegram_auth_rejected, emit_telegram_code_sent,
    emit_telegram_data_reset, emit_telegram_session_ended,
};
use crate::secrets::{TelegramSecretKey, TelegramSecretVault};

/// Marker written to the persistent session key after authorization.
pub const TDLIB_SESSION_MARKER: &str = "tdlib-ready";

#[derive(Clone)]
enum TdlibCommand {
    Step(TelegramAuthStep),
    LoadChats,
    OpenChat(String),
    SendText {
        conversation_id: String,
        body: String,
    },
    Resend {
        conversation_id: String,
        message_id: String,
    },
    /// Ask TDLib to close. The worker exits after `authorizationStateClosed`.
    Close,
}

/// How often a waiter checks that closing workers have exited.
const WORKER_POLL: Duration = Duration::from_millis(50);

/// Longest wait for an old client to release the database before a new one
/// opens it.
const RETIRE_TIMEOUT: Duration = Duration::from_secs(10);

struct LiveInbox {
    authorized: bool,
    /// This app asked TDLib to close (shutdown, Cancel, Try again). A close
    /// without this flag came from elsewhere, for example a remote logout.
    closing: bool,
    /// A live session started to close without our request. Reported once
    /// the client is fully closed (qa R74).
    ended_elsewhere: bool,
    directory: ChatDirectory,
    names: NameBook,
}

/// Owns the command sink into the current worker and the workers' done flags.
///
/// A worker never drops its TDLib client without `close`: an unclean exit
/// aborts the process at exit and can damage the TDLib database.
pub struct TdlibRuntime {
    commands: Option<UnboundedSender<TdlibCommand>>,
    slots: WorkerSlots,
}

impl TdlibRuntime {
    #[must_use]
    pub fn new() -> Self {
        Self {
            commands: None,
            slots: WorkerSlots::new(),
        }
    }

    /// Close the current worker's TDLib client. The next command starts a new
    /// worker, which waits until this one released the database.
    pub fn stop(&mut self) {
        if let Some(commands) = self.commands.take() {
            let _ = commands.send(TdlibCommand::Close);
        }
        self.slots.retire_current();
    }

    /// Close every TDLib client, then emit `Stopped` once all workers exited.
    pub fn shutdown(&mut self, events: &EventTx) {
        self.stop();
        self.slots.shut_down();
        let workers = self.slots.all();
        let events = events.clone();
        tokio::spawn(async move {
            // Exit only when no thread is inside TDLib any more.
            while !all_done(&workers) || !receiver_idle() {
                tokio::time::sleep(WORKER_POLL).await;
            }
            data_dir::remove_this_process_session_dir();
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

    pub fn send_text(
        &mut self,
        conversation_id: String,
        body: String,
        secrets: Arc<dyn TelegramSecretVault>,
        source: TelegramApiSource,
        events: &EventTx,
    ) {
        self.enqueue(
            TdlibCommand::SendText {
                conversation_id,
                body,
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
        secrets: Arc<dyn TelegramSecretVault>,
        source: TelegramApiSource,
        events: &EventTx,
    ) {
        self.enqueue(
            TdlibCommand::Resend {
                conversation_id,
                message_id,
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
        let slots = &mut self.slots;
        let sent = super::send_or_respawn(&mut self.commands, command, || {
            let (done, wait_for) = slots.start().unwrap_or_default();
            spawn_tdlib_worker(
                Arc::clone(&secrets),
                source.clone(),
                events.clone(),
                done,
                wait_for,
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
}

static DISPATCHER: OnceLock<Arc<Dispatcher>> = OnceLock::new();

impl Dispatcher {
    fn get() -> Arc<Self> {
        Arc::clone(DISPATCHER.get_or_init(|| {
            let dispatcher = Arc::new(Self {
                router: Mutex::new(Router::new()),
                wake: Condvar::new(),
            });
            let receiving = Arc::clone(&dispatcher);
            let spawned = thread::Builder::new()
                .name("thinwire-tdlib-recv".into())
                .spawn(move || receiving.receive_loop());
            if let Err(error) = spawned {
                tracing::warn!(%error, "tdlib receive thread did not start");
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
    DISPATCHER
        .get()
        .is_none_or(|dispatcher| dispatcher.lock().idle())
}

fn spawn_tdlib_worker(
    secrets: Arc<dyn TelegramSecretVault>,
    source: TelegramApiSource,
    events: EventTx,
    done: DoneFlag,
    wait_for: Vec<DoneFlag>,
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

        if tdlib_rs::functions::set_log_verbosity_level(1, client_id)
            .await
            .is_err()
        {
            tracing::info!("tdlib log verbosity was not applied");
        }

        let mut live = LiveInbox {
            authorized: false,
            closing: false,
            ended_elsewhere: false,
            directory: ChatDirectory::new(),
            names: NameBook::new(),
        };
        let mut commands = Some(cmd_rx);

        loop {
            tokio::select! {
                command = next_command(&mut commands) => {
                    // A dropped sender means the runtime let go of this worker.
                    // Close TDLib in both cases; keep reading updates until Closed.
                    let command = command.unwrap_or_else(|| {
                        commands = None;
                        TdlibCommand::Close
                    });
                    if live.closing {
                        continue;
                    }
                    match command {
                        TdlibCommand::Close => {
                            live.closing = true;
                            request_close(client_id).await;
                        }
                        TdlibCommand::Step(step) => {
                            apply_step(client_id, step, secrets.as_ref(), &events).await;
                        }
                        TdlibCommand::LoadChats => {
                            load_main_chats(client_id, live.authorized, &events).await;
                        }
                        TdlibCommand::OpenChat(conversation_id) => {
                            open_chat(client_id, &conversation_id, &live, &events).await;
                        }
                        TdlibCommand::SendText { conversation_id, body } => {
                            send_text(client_id, &conversation_id, &body, &live, &events).await;
                        }
                        TdlibCommand::Resend { conversation_id, message_id } => {
                            resend(client_id, &conversation_id, &message_id, &live, &events).await;
                        }
                    }
                }
                update = update_rx.recv() => {
                    let Some(update) = update else {
                        break;
                    };
                    let closed = apply_update(client_id, update, secrets.as_ref(), &source, &events, &mut live).await;
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
) {
    match step {
        TelegramAuthStep::ApiCredentials => {
            emit_status(
                events,
                ProtocolId::Telegram,
                AdapterStatus::Connecting,
                "TDLib client started. Waiting for authorization state.",
            );
        }
        TelegramAuthStep::Phone => {
            let Some(phone) = secrets.get_secret(TelegramSecretKey::Phone) else {
                emit_telegram_auth(events, TelegramAuthPhase::Failed);
                emit_status(
                    events,
                    ProtocolId::Telegram,
                    AdapterStatus::Error,
                    "Phone number is missing from the secret store.",
                );
                return;
            };
            if let Err(error) =
                tdlib_rs::functions::set_authentication_phone_number(phone, None, client_id).await
            {
                reject_step(events, &error);
                emit_status(
                    events,
                    ProtocolId::Telegram,
                    AdapterStatus::Error,
                    "Telegram rejected the phone number. Check the number or Cancel.",
                );
            }
        }
        TelegramAuthStep::Code => {
            let Some(code) = secrets.get_secret(TelegramSecretKey::Code) else {
                emit_telegram_auth(events, TelegramAuthPhase::Failed);
                emit_status(
                    events,
                    ProtocolId::Telegram,
                    AdapterStatus::Error,
                    "Login code is missing from the secret store.",
                );
                return;
            };
            if let Err(error) =
                tdlib_rs::functions::check_authentication_code(code, client_id).await
            {
                reject_step(events, &error);
                emit_status(
                    events,
                    ProtocolId::Telegram,
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
                reject_step(events, &error);
                emit_status(
                    events,
                    ProtocolId::Telegram,
                    AdapterStatus::Error,
                    "Telegram rejected the 2FA password. Try again or Cancel.",
                );
            }
        }
    }
}

/// Report why TDLib refused a login step, then the `Failed` phase.
/// Only the error name and code are read; the typed value is never echoed.
fn reject_step(events: &EventTx, error: &tdlib_rs::types::Error) {
    log_tdlib_error("login step", error);
    let reason: TelegramAuthError =
        super::auth_error::auth_error_from_tdlib(error.code, &error.message);
    emit_telegram_auth_rejected(events, reason);
    emit_telegram_auth(events, TelegramAuthPhase::Failed);
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
) -> bool {
    match update {
        tdlib_rs::enums::Update::AuthorizationState(state) => {
            let closed = matches!(
                state.authorization_state,
                tdlib_rs::enums::AuthorizationState::Closed
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
            return closed;
        }
        tdlib_rs::enums::Update::User(update) => {
            live.names.remember_user(
                update.user.id,
                &update.user.first_name,
                &update.user.last_name,
            );
        }
        other => apply_chat_update(other, live, events),
    }
    false
}

async fn apply_authorization(
    client_id: i32,
    state: tdlib_rs::enums::AuthorizationState,
    secrets: &dyn TelegramSecretVault,
    source: &TelegramApiSource,
    events: &EventTx,
    live: &mut LiveInbox,
) {
    match state {
        tdlib_rs::enums::AuthorizationState::WaitTdlibParameters => {
            set_parameters(client_id, secrets, source, events).await;
        }
        tdlib_rs::enums::AuthorizationState::WaitPhoneNumber => {
            // A saved session that lands here expired or was revoked. Drop the
            // marker so the next launch does not try to resume it again.
            if secrets.get_secret(TelegramSecretKey::Session).is_some() {
                secrets.set_secret(TelegramSecretKey::Session, "");
                super::request_secret_flush(events);
            }
            emit_telegram_auth(events, TelegramAuthPhase::NeedPhone);
            emit_status(
                events,
                ProtocolId::Telegram,
                AdapterStatus::Connecting,
                "Telegram needs a phone number. Values stay in the secret store.",
            );
        }
        tdlib_rs::enums::AuthorizationState::WaitCode(state) => {
            emit_telegram_code_sent(events, code_via(&state.code_info.r#type));
            emit_telegram_auth(events, TelegramAuthPhase::NeedCode);
            emit_status(
                events,
                ProtocolId::Telegram,
                AdapterStatus::Connecting,
                "Telegram sent a login code. Enter it here. It is not logged.",
            );
        }
        tdlib_rs::enums::AuthorizationState::WaitPassword(_) => {
            emit_telegram_auth(events, TelegramAuthPhase::NeedTwoFactor);
            emit_status(
                events,
                ProtocolId::Telegram,
                AdapterStatus::Connecting,
                "Telegram needs the account password (two-step verification).",
            );
        }
        tdlib_rs::enums::AuthorizationState::Ready => {
            secrets.set_secret(TelegramSecretKey::Session, TDLIB_SESSION_MARKER);
            super::request_secret_flush(events);
            live.authorized = true;
            emit_telegram_auth(events, TelegramAuthPhase::Ready);
            emit_status(
                events,
                ProtocolId::Telegram,
                AdapterStatus::Ready,
                "Telegram is ready. Loading the chat list.",
            );
            for conversation in live.directory.listed() {
                emit_conversation(events, conversation);
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
            if was_authorized && !live.closing {
                live.ended_elsewhere = true;
            }
            emit_status(
                events,
                ProtocolId::Telegram,
                AdapterStatus::Stubbed,
                "Telegram session closed.",
            );
        }
    }
}

fn apply_chat_update(update: tdlib_rs::enums::Update, live: &mut LiveInbox, events: &EventTx) {
    let emit = live.authorized;
    match update {
        tdlib_rs::enums::Update::NewChat(update) => {
            publish(events, emit, note_chat(&mut live.directory, &update.chat));
        }
        tdlib_rs::enums::Update::ChatTitle(update) => {
            publish(
                events,
                emit,
                live.directory.set_title(update.chat_id, &update.title),
            );
        }
        tdlib_rs::enums::Update::ChatPosition(update) => {
            if matches!(update.position.list, tdlib_rs::enums::ChatList::Main) {
                publish(
                    events,
                    emit,
                    live.directory
                        .set_main_order(update.chat_id, update.position.order),
                );
            }
        }
        tdlib_rs::enums::Update::ChatReadInbox(update) => {
            publish(
                events,
                emit,
                live.directory
                    .set_unread(update.chat_id, update.unread_count),
            );
        }
        tdlib_rs::enums::Update::ChatLastMessage(update) => {
            if let Some(order) = main_order(&update.positions) {
                publish(
                    events,
                    emit,
                    live.directory.set_main_order(update.chat_id, order),
                );
            }
            if let Some(message) = update.last_message.as_ref() {
                let preview = message_body(&message.content);
                publish(
                    events,
                    emit,
                    live.directory
                        .set_preview(update.chat_id, &preview, i64::from(message.date)),
                );
                if emit {
                    emit_mapped_message(events, message, live, None);
                }
            } else {
                publish(
                    events,
                    emit,
                    live.directory.set_preview(update.chat_id, "", 0),
                );
            }
        }
        tdlib_rs::enums::Update::NewMessage(update) => {
            let preview = message_body(&update.message.content);
            let chat_id = update.message.chat_id;
            let at = i64::from(update.message.date);
            if emit {
                emit_mapped_message(events, &update.message, live, None);
            }
            publish(
                events,
                emit,
                live.directory.set_preview(chat_id, &preview, at),
            );
        }
        tdlib_rs::enums::Update::MessageSendSucceeded(update) => {
            if emit {
                let old_id = inbox::message_id(update.message.chat_id, update.old_message_id);
                emit_mapped_message(events, &update.message, live, Some(old_id));
                emit_status(
                    events,
                    ProtocolId::Telegram,
                    AdapterStatus::Ready,
                    "Message sent.",
                );
            }
        }
        tdlib_rs::enums::Update::MessageSendFailed(update) => {
            if emit {
                log_tdlib_error("sendMessage (update)", &update.error);
                let old_id = inbox::message_id(update.message.chat_id, update.old_message_id);
                emit_mapped_message(events, &update.message, live, Some(old_id));
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
        tdlib_rs::enums::Update::DeleteMessages(update) => {
            // Cache eviction can be fetched again. Rows drop only when the chat lost them.
            if !emit || update.from_cache {
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
            if !emit {
                return;
            }
            emit_message_body(
                events,
                ProtocolId::Telegram,
                inbox::conversation_id(update.chat_id),
                inbox::message_id(update.chat_id, update.message_id),
                message_body(&update.new_content),
            );
        }
        _ => {}
    }
}

fn publish(events: &EventTx, emit: bool, effect: Option<ChatEffect>) {
    if emit {
        emit_effect(events, effect);
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

fn note_chat(directory: &mut ChatDirectory, chat: &tdlib_rs::types::Chat) -> Option<ChatEffect> {
    let order = main_order(&chat.positions).unwrap_or(0);
    let preview = chat
        .last_message
        .as_ref()
        .map(|message| message_body(&message.content))
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
        },
    )
}

fn main_order(positions: &[tdlib_rs::types::ChatPosition]) -> Option<i64> {
    positions
        .iter()
        .find(|position| matches!(position.list, tdlib_rs::enums::ChatList::Main))
        .map(|position| position.order)
}

fn emit_mapped_message(
    events: &EventTx,
    message: &tdlib_rs::types::Message,
    live: &LiveInbox,
    replace_old: Option<String>,
) {
    let party = match &message.sender_id {
        tdlib_rs::enums::MessageSender::User(user) => MessageParty::User(user.user_id),
        tdlib_rs::enums::MessageSender::Chat(chat) => MessageParty::Chat(chat.chat_id),
    };
    let mapped = inbox::to_chat_message(
        &InboxMessage {
            chat_id: message.chat_id,
            message_id: message.id,
            outgoing: message.is_outgoing,
            party,
            body: message_body(&message.content),
            sent_at: i64::from(message.date),
            delivery: match &message.sending_state {
                None => Delivery::Sent,
                Some(tdlib_rs::enums::MessageSendingState::Pending(_)) => Delivery::Pending,
                Some(tdlib_rs::enums::MessageSendingState::Failed(_)) => Delivery::Failed,
            },
        },
        &live.names,
        live.directory.title(message.chat_id),
    );
    if let Some(old_id) = replace_old.filter(|old_id| *old_id != mapped.id) {
        emit_message_replaced(events, old_id, mapped);
        return;
    }
    emit_message(events, mapped);
}

fn message_body(content: &tdlib_rs::enums::MessageContent) -> String {
    match content {
        tdlib_rs::enums::MessageContent::MessageText(text) => text.text.text.clone(),
        tdlib_rs::enums::MessageContent::MessagePhoto(_) => "Photo".into(),
        tdlib_rs::enums::MessageContent::MessageSticker(_) => "Sticker".into(),
        tdlib_rs::enums::MessageContent::MessageVideo(_) => "Video".into(),
        tdlib_rs::enums::MessageContent::MessageAnimation(_) => "Animation".into(),
        tdlib_rs::enums::MessageContent::MessageVoiceNote(_) => "Voice message".into(),
        tdlib_rs::enums::MessageContent::MessageDocument(_) => "File".into(),
        _ => "Message".into(),
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

async fn open_chat(client_id: i32, conversation_id: &str, live: &LiveInbox, events: &EventTx) {
    if !live.authorized {
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
    emit_status(
        events,
        ProtocolId::Telegram,
        AdapterStatus::Ready,
        "Loading recent messages.",
    );
    load_history(client_id, chat_id, live, events).await;
    emit_history_loaded(events, ProtocolId::Telegram, conversation_id);
}

async fn load_history(client_id: i32, chat_id: i64, live: &LiveInbox, events: &EventTx) {
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
            let viewed: Vec<i64> = messages.iter().map(|message| message.id).collect();
            for message in inbox::chronological(messages) {
                emit_mapped_message(events, &message, live, None);
            }
            if !viewed.is_empty()
                && let Err(error) =
                    tdlib_rs::functions::view_messages(chat_id, viewed, None, true, client_id).await
            {
                log_tdlib_error("viewMessages", &error);
                emit_status(
                    events,
                    ProtocolId::Telegram,
                    AdapterStatus::Error,
                    format!("Could not mark messages read (TDLib {}).", error.code),
                );
                return;
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

async fn send_text(
    client_id: i32,
    conversation_id: &str,
    body: &str,
    live: &LiveInbox,
    events: &EventTx,
) {
    if !live.authorized {
        emit_status(
            events,
            ProtocolId::Telegram,
            AdapterStatus::Error,
            "Telegram is not ready. Send after authorization.",
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
    let text = body.trim();
    if text.is_empty() {
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
    match tdlib_rs::functions::send_message(chat_id, None, None, None, content, client_id).await {
        Ok(tdlib_rs::enums::Message::Message(message)) => {
            emit_mapped_message(events, &message, live, None);
        }
        Err(error) => {
            log_tdlib_error("sendMessage", &error);
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
    live: &LiveInbox,
    events: &EventTx,
) {
    let failed = || {
        emit_message_delivery(
            events,
            ProtocolId::Telegram,
            conversation_id,
            message_id,
            Delivery::Failed,
        );
    };
    if !live.authorized {
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
                    emit_mapped_message(events, &message, live, Some(message_id.to_string()));
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
) {
    let (api_id, api_hash) = match require_resolved_api(secrets, source) {
        Ok(pair) => pair,
        Err(_) => {
            emit_telegram_auth(events, TelegramAuthPhase::Failed);
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
        emit_telegram_auth(events, TelegramAuthPhase::Failed);
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
        emit_telegram_auth_rejected(events, TelegramAuthError::ClientSetup { code: 0 });
        emit_telegram_auth(events, TelegramAuthPhase::Failed);
        emit_status(
            events,
            ProtocolId::Telegram,
            AdapterStatus::Error,
            "Telegram could not start (keychain still opening).",
        );
        return;
    }
    let dir = match tdlib_data_dir(secrets.persists()) {
        Ok(dir) => dir,
        Err(error) => {
            tracing::warn!(%error, "no private folder for the Telegram session");
            emit_telegram_auth_rejected(events, TelegramAuthError::ClientSetup { code: 0 });
            emit_telegram_auth(events, TelegramAuthPhase::Failed);
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
        emit_telegram_data_reset(events, name);
    }
    if let Err(error) = result {
        // No login step can run now. Stop the flow; the UI must not show
        // the phone step (TDLib would answer "call setTdlibParameters first").
        log_tdlib_error("setTdlibParameters", &error);
        emit_telegram_auth_rejected(events, TelegramAuthError::ClientSetup { code: error.code });
        emit_telegram_auth(events, TelegramAuthPhase::Failed);
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
fn tdlib_data_dir(persists: bool) -> std::io::Result<PathBuf> {
    Ok(if let Some(dir) = std::env::var_os("THINWIRE_TDLIB_DIR") {
        PathBuf::from(dir)
    } else if !persists {
        data_dir::this_process_session_dir()?
    } else {
        let mut base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(|home| PathBuf::from(home).join(".local").join("share"))
            })
            .unwrap_or_else(std::env::temp_dir);
        base.push("thinwire");
        base.push("tdlib");
        base
    })
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
