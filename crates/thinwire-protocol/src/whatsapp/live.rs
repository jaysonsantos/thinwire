//! Live backend of the WhatsApp link. Compiled only with `whatsapp-web`.
//!
//! The link owner ([`super::link`]) calls [`LiveBackend`] one step at a time.
//! This module holds no lifecycle state. `Bot` runs on a tokio task; the egui
//! thread never calls into this module. QR payloads and pair codes are
//! events, not command fields, and are not logged.

use std::sync::Arc;

use whatsapp_rust::bot::{Bot, BotHandle};
use whatsapp_rust::pair_code::PairCodeOptions;
use whatsapp_rust::send::SendError;
use whatsapp_rust::store::SqliteStore;
use whatsapp_rust::types::events::{Event, EventKind};
use whatsapp_rust::wacore::proto_helpers::{MessageBuilderExt, MessageExt};
use whatsapp_rust::waproto::whatsapp as wa;
use whatsapp_rust::{Client, ClientError, Jid};

use super::inbox::{HistoryChat, WaMessage};
use super::link::{Callbacks, LinkBackend, StartError, Started};
use super::path::{
    prepare_session_dir, remove_device_store, restrict_store_file, revoked_marker,
    whatsapp_device_store_path,
};
use super::session::{LinkEvent, SendFailure, SendFuture, WhatsAppSender};
use crate::adapter::RedactedPairingSecret;

/// whatsapp-rust client work for the link owner. No state of its own.
pub(super) struct LiveBackend;

impl LinkBackend for LiveBackend {
    type Bot = BotHandle;

    async fn start(
        &self,
        _generation: u64,
        phone: Option<String>,
        callbacks: Callbacks,
    ) -> Result<Started<BotHandle>, StartError> {
        let path = whatsapp_device_store_path().map_err(|()| StartError::DataDir)?;
        let parent = path
            .parent()
            .map(std::path::Path::to_path_buf)
            .ok_or(StartError::DataDir)?;
        tokio::task::spawn_blocking(move || prepare_session_dir(&parent))
            .await
            .ok()
            .and_then(Result::ok)
            .ok_or(StartError::Store)?;
        let db = path.to_str().ok_or(StartError::Store)?;
        let backend = SqliteStore::new(db).await.map_err(|_| StartError::Store)?;
        let _ = restrict_store_file(&path);
        let bot = build_bot(backend, digits_only(phone), callbacks)
            .await
            .map_err(|()| StartError::Build)?;
        let client = bot.client();
        Ok(Started {
            bot: bot.spawn(),
            sender: Arc::new(LiveSender { client }),
        })
    }

    async fn stop(&self, bot: BotHandle) {
        bot.shutdown().await;
    }

    async fn delete_store(&self) -> Result<(), ()> {
        let path = whatsapp_device_store_path()?;
        tokio::task::spawn_blocking(move || remove_device_store(&path))
            .await
            .map_err(|_| ())?
            .map_err(|_| ())
    }

    async fn mark_revoked(&self) {
        let Ok(path) = whatsapp_device_store_path() else {
            return;
        };
        let marker = revoked_marker(&path);
        let _ = tokio::task::spawn_blocking(move || std::fs::write(marker, b"")).await;
    }

    async fn is_revoked(&self) -> bool {
        let Ok(path) = whatsapp_device_store_path() else {
            return false;
        };
        let marker = revoked_marker(&path);
        tokio::task::spawn_blocking(move || marker.exists())
            .await
            .unwrap_or(false)
    }
}

fn digits_only(phone: Option<String>) -> Option<String> {
    let raw = phone?;
    let digits: String = raw.chars().filter(|ch| ch.is_ascii_digit()).collect();
    if digits.is_empty() {
        None
    } else {
        Some(digits)
    }
}

/// Event kinds the inbox reads. Everything else is skipped by the bus.
const INBOX_EVENTS: &[EventKind] = &[
    EventKind::PairingQrCode,
    EventKind::PairingCode,
    EventKind::PairingCodeError,
    EventKind::PairingQrCodesExhausted,
    EventKind::PairSuccess,
    EventKind::PairError,
    EventKind::Connected,
    EventKind::Disconnected,
    EventKind::LoggedOut,
    EventKind::TemporaryBan,
    EventKind::HistorySync,
    EventKind::Messages,
];

async fn build_bot(
    backend: SqliteStore,
    phone_number: Option<String>,
    callbacks: Callbacks,
) -> Result<Bot, ()> {
    let mut builder =
        Bot::builder()
            .with_backend(backend)
            .on_event_for(INBOX_EVENTS, move |event, _client| {
                let callbacks = callbacks.clone();
                async move {
                    if let Some(link_event) = map_event(&event) {
                        callbacks.send(link_event);
                    }
                }
            });
    if let Some(phone_number) = phone_number {
        builder = builder.with_pair_code(PairCodeOptions {
            phone_number,
            ..Default::default()
        });
    }
    builder.build().await.map_err(|_| ())
}

fn map_event(event: &Event) -> Option<LinkEvent> {
    Some(match event {
        Event::PairingQrCode(qr) => LinkEvent::Qr(RedactedPairingSecret::new(qr.code.clone())),
        Event::PairingCode(code) => {
            LinkEvent::PairCode(RedactedPairingSecret::new(code.code.clone()))
        }
        Event::PairingCodeError(error)
            if error
                .rejection
                .is_some_and(|rejection| rejection.is_throttled()) =>
        {
            LinkEvent::PairThrottled
        }
        Event::PairingCodeError(_) | Event::PairError(_) => LinkEvent::PairFailed,
        Event::PairingQrCodesExhausted(_) => LinkEvent::QrExhausted,
        Event::PairSuccess(_) => LinkEvent::Paired,
        Event::Connected(_) => LinkEvent::Connected,
        Event::Disconnected(_) => LinkEvent::Disconnected,
        Event::LoggedOut(_) => LinkEvent::LoggedOut,
        Event::TemporaryBan(_) => LinkEvent::TemporaryBan,
        Event::HistorySync(sync) => map_history(sync.get()?),
        Event::Messages(batch) => LinkEvent::Messages(
            batch
                .iter()
                .filter_map(|inbound| {
                    let info = &inbound.info;
                    Some(WaMessage {
                        chat_jid: info.source.chat.to_string(),
                        id: info.id.to_string(),
                        from_me: info.source.is_from_me,
                        sender_name: non_empty(info.push_name.as_str()),
                        sender_jid: Some(info.source.sender.to_non_ad().to_string()),
                        body: body_of(&inbound.message)?,
                        timestamp: info.timestamp.timestamp(),
                    })
                })
                .collect(),
        ),
        _ => return None,
    })
}

fn map_history(sync: &wa::HistorySync) -> LinkEvent {
    let chats = sync
        .conversations
        .iter()
        .map(|conversation| {
            let jid = conversation.id.clone();
            let messages = conversation
                .messages
                .iter()
                .filter_map(|row| map_history_message(&jid, &row.message))
                .collect();
            HistoryChat {
                name: conversation
                    .name
                    .as_deref()
                    .or(conversation.display_name.as_deref())
                    .and_then(non_empty),
                unread: conversation.unread_count.unwrap_or(0),
                timestamp: conversation
                    .conversation_timestamp
                    .or(conversation.last_msg_timestamp)
                    .and_then(|ts| i64::try_from(ts).ok())
                    .unwrap_or(0),
                jid,
                messages,
            }
        })
        .collect();
    let push_names = sync
        .pushnames
        .iter()
        .filter_map(|row| Some((row.id.clone()?, row.pushname.clone()?)))
        .collect();
    LinkEvent::History { chats, push_names }
}

fn map_history_message(chat_jid: &str, info: &wa::WebMessageInfo) -> Option<WaMessage> {
    let key = &info.key;
    let from_me = key.from_me.unwrap_or(false);
    let sender_jid = key
        .participant
        .clone()
        .or_else(|| info.participant.clone())
        .or_else(|| (!from_me).then(|| chat_jid.to_string()));
    Some(WaMessage {
        chat_jid: chat_jid.to_string(),
        id: key.id.clone()?,
        from_me,
        sender_name: info.push_name.as_deref().and_then(non_empty),
        sender_jid,
        body: body_of(info.message.as_option()?)?,
        timestamp: info
            .message_timestamp
            .and_then(|ts| i64::try_from(ts).ok())
            .unwrap_or(0),
    })
}

/// Plain text, a media caption, or a short media label. Protocol-only
/// messages (reactions, key distribution, receipts) return `None`.
fn body_of(message: &wa::Message) -> Option<String> {
    let base = message.get_base_message();
    if let Some(text) = base.text_content().or_else(|| base.get_caption()) {
        return non_empty(text);
    }
    let label = if base.image_message.is_set() {
        "[photo]"
    } else if base.video_message.is_set() {
        "[video]"
    } else if base.audio_message.is_set() {
        "[audio]"
    } else if base.document_message.is_set() {
        "[document]"
    } else if base.sticker_message.is_set() {
        "[sticker]"
    } else if base.contact_message.is_set() {
        "[contact]"
    } else if base.location_message.is_set() {
        "[location]"
    } else {
        return None;
    };
    Some(label.to_string())
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Class of a send error. The error text is dropped: it can hold JIDs.
fn classify_send_error(error: &SendError) -> SendFailure {
    match error {
        SendError::NotLoggedIn | SendError::Client(ClientError::NotLoggedIn) => {
            SendFailure::Unlinked
        }
        SendError::Client(_) | SendError::Iq(_) => SendFailure::Network,
        _ => SendFailure::Rejected,
    }
}

/// Sends plain text through the linked-device client.
struct LiveSender {
    client: Arc<Client>,
}

impl WhatsAppSender for LiveSender {
    fn send_text<'a>(&'a self, chat_jid: &'a str, body: &'a str) -> SendFuture<'a> {
        Box::pin(async move {
            let jid: Jid = chat_jid.parse().map_err(|_| SendFailure::Rejected)?;
            self.client
                .send_message(jid, wa::Message::text(body))
                .await
                .map(|sent| sent.message_id)
                .map_err(|error| classify_send_error(&error))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_errors_map_to_failure_classes_without_text() {
        assert_eq!(
            classify_send_error(&SendError::Client(ClientError::NotConnected)),
            SendFailure::Network
        );
        assert_eq!(
            classify_send_error(&SendError::NotLoggedIn),
            SendFailure::Unlinked
        );
        assert_eq!(
            classify_send_error(&SendError::Client(ClientError::NotLoggedIn)),
            SendFailure::Unlinked
        );
        assert_eq!(
            classify_send_error(&SendError::InvalidRequest("111@s.whatsapp.net".into())),
            SendFailure::Rejected
        );
        let debug = format!("{:?}", SendFailure::Rejected);
        assert!(!debug.contains("111"));
    }

    #[test]
    fn body_uses_text_caption_or_media_label() {
        assert_eq!(body_of(&wa::Message::text("  hi ")).as_deref(), Some("hi"));
        let image = wa::Message {
            image_message: whatsapp_rust::waproto::buffa::MessageField::some(
                wa::message::ImageMessage::default(),
            ),
            ..Default::default()
        };
        assert_eq!(body_of(&image).as_deref(), Some("[photo]"));
        assert_eq!(body_of(&wa::Message::default()), None);
    }
}
