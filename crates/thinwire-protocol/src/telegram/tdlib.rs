//! Live `tdlib-rs` client. Compiled only with `--features telegram-tdlib`.
//!
//! FFI and network I/O stay on a dedicated receive thread plus tokio tasks.
//! Credential values are read from the vault and never logged.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use tokio::sync::mpsc::UnboundedSender;

use super::credentials::{TelegramApiSource, parse_resolved_api_id, require_resolved_api};
use super::inbox::{
    self, ChatDirectory, ChatEffect, ChatSeed, InboxMessage, MessageParty, NameBook,
};
use crate::adapter::{
    AdapterStatus, Delivery, EventTx, ProtocolId, TelegramAuthError, TelegramAuthPhase,
    TelegramAuthStep, TelegramCodeVia, emit_chat_list_loaded, emit_conversation,
    emit_conversation_removed, emit_history_loaded, emit_message, emit_message_body,
    emit_message_delivery, emit_message_replaced, emit_messages_removed, emit_status,
    emit_telegram_auth, emit_telegram_auth_rejected, emit_telegram_code_sent,
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
}

struct LiveInbox {
    authorized: bool,
    directory: ChatDirectory,
    names: NameBook,
}

/// Owns the TDLib client id and the command sink into the worker.
pub struct TdlibRuntime {
    commands: Option<UnboundedSender<TdlibCommand>>,
    generation: Arc<AtomicU64>,
}

impl TdlibRuntime {
    #[must_use]
    pub fn new() -> Self {
        Self {
            commands: None,
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn stop(&mut self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.commands = None;
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
        let born = self.generation.load(Ordering::SeqCst);
        let generation = Arc::clone(&self.generation);
        let sent = super::send_or_respawn(&mut self.commands, command, || {
            spawn_tdlib_worker(
                Arc::clone(&secrets),
                source.clone(),
                events.clone(),
                Arc::clone(&generation),
                born,
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

fn spawn_tdlib_worker(
    secrets: Arc<dyn TelegramSecretVault>,
    source: TelegramApiSource,
    events: EventTx,
    generation: Arc<AtomicU64>,
    born: u64,
) -> UnboundedSender<TdlibCommand> {
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let (update_tx, mut update_rx) = tokio::sync::mpsc::unbounded_channel();

    let client_id = tdlib_rs::create_client();
    let receive_generation = Arc::clone(&generation);
    thread::Builder::new()
        .name("thinwire-tdlib-recv".into())
        .spawn(move || {
            // `receive` returns None on timeout and after it hands a response
            // to the request observer. Keep looping so chat updates and
            // in-flight calls are not dropped when the UI is idle.
            loop {
                if receive_generation.load(Ordering::SeqCst) != born {
                    break;
                }
                match tdlib_rs::receive() {
                    Some((update, id)) if id == client_id => {
                        if update_tx.send(update).is_err() {
                            break;
                        }
                    }
                    Some(_) | None => {}
                }
            }
        })
        .ok();

    tokio::spawn(async move {
        if tdlib_rs::functions::set_log_verbosity_level(1, client_id)
            .await
            .is_err()
        {
            tracing::info!("tdlib log verbosity was not applied");
        }

        let mut live = LiveInbox {
            authorized: false,
            directory: ChatDirectory::new(),
            names: NameBook::new(),
        };

        loop {
            tokio::select! {
                command = cmd_rx.recv() => {
                    let Some(command) = command else {
                        break;
                    };
                    if generation.load(Ordering::SeqCst) != born {
                        break;
                    }
                    match command {
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
                    if generation.load(Ordering::SeqCst) != born {
                        break;
                    }
                    apply_update(client_id, update, secrets.as_ref(), &source, &events, &mut live).await;
                }
            }
        }
    });

    cmd_tx
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
    let reason: TelegramAuthError =
        super::auth_error::auth_error_from_tdlib(error.code, &error.message);
    emit_telegram_auth_rejected(events, reason);
    emit_telegram_auth(events, TelegramAuthPhase::Failed);
}

fn code_via(kind: &tdlib_rs::enums::AuthenticationCodeType) -> TelegramCodeVia {
    use tdlib_rs::enums::AuthenticationCodeType as Kind;
    match kind {
        Kind::TelegramMessage(_) => TelegramCodeVia::TelegramApp,
        Kind::Sms(_) | Kind::SmsWord(_) | Kind::SmsPhrase(_) => TelegramCodeVia::Sms,
        Kind::Call(_) | Kind::FlashCall(_) | Kind::MissedCall(_) => TelegramCodeVia::Call,
        _ => TelegramCodeVia::Other,
    }
}

async fn apply_update(
    client_id: i32,
    update: tdlib_rs::enums::Update,
    secrets: &dyn TelegramSecretVault,
    source: &TelegramApiSource,
    events: &EventTx,
    live: &mut LiveInbox,
) {
    match update {
        tdlib_rs::enums::Update::AuthorizationState(state) => {
            apply_authorization(
                client_id,
                state.authorization_state,
                secrets,
                source,
                events,
                live,
            )
            .await;
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
            live.authorized = false;
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
        Err(error) => emit_status(
            events,
            ProtocolId::Telegram,
            AdapterStatus::Error,
            format!("Could not load Telegram chats (TDLib {}).", error.code),
        ),
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
        Err(error) => emit_status(
            events,
            ProtocolId::Telegram,
            AdapterStatus::Error,
            format!("Could not load messages (TDLib {}).", error.code),
        ),
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
        Err(error) => emit_status(
            events,
            ProtocolId::Telegram,
            AdapterStatus::Error,
            format!("Telegram did not send the message (TDLib {}).", error.code),
        ),
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
    let database_directory = tdlib_data_dir().display().to_string();
    let encryption_key = ensure_db_key(secrets, events);
    if tdlib_rs::functions::set_tdlib_parameters(
        false,
        database_directory,
        String::new(),
        encryption_key,
        true,
        true,
        true,
        false,
        api_id,
        api_hash,
        "en".into(),
        "thinwire".into(),
        String::new(),
        env!("CARGO_PKG_VERSION").into(),
        client_id,
    )
    .await
    .is_err()
    {
        emit_status(
            events,
            ProtocolId::Telegram,
            AdapterStatus::Error,
            "TDLib rejected the client parameters. Check api_id and api_hash.",
        );
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

fn tdlib_data_dir() -> PathBuf {
    let path = if let Some(dir) = std::env::var_os("THINWIRE_TDLIB_DIR") {
        PathBuf::from(dir)
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
    };
    let _ = std::fs::create_dir_all(&path);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700));
    }
    path
}
