// SPDX-License-Identifier: AGPL-3.0-only
//! Live backend of the WhatsApp link. Compiled only with `whatsapp-web`.
//!
//! The link owner ([`super::link`]) calls [`LiveBackend`] one step at a time.
//! This module holds no lifecycle state. `Bot` runs on a tokio task; the egui
//! thread never calls into this module. QR payloads and pair codes are
//! events, not command fields, and are not logged.

use std::sync::Arc;

use whatsapp_rust::bot::{Bot, BotHandle, BotRunOutcome, BotShutdownReport};
use whatsapp_rust::pair_code::PairCodeOptions;
use whatsapp_rust::send::SendError;
use whatsapp_rust::store::SqliteStore;
use whatsapp_rust::store::error::StoreError;
use whatsapp_rust::types::events::{Event, EventKind};
use whatsapp_rust::wacore::proto_helpers::{MessageBuilderExt, MessageExt};
use whatsapp_rust::wacore::store::DevicePropsOverride;
use whatsapp_rust::waproto::whatsapp as wa;
use whatsapp_rust::{Client, ClientError, Jid, RunCompletionReason};

use super::inbox::{HistoryChat, MEDIA_MAX_BYTES, MediaRef, Mute, Sticker, Voice, WaMessage};
use super::link::{Callbacks, LinkBackend, StartError, Started};
use super::path::{
    prepare_session_dir, remove_device_store, restrict_store_file, revoked_marker,
    whatsapp_device_store_path,
};
use super::session::{DownloadFuture, LinkEvent, SendFailure, SendFuture, WhatsAppSender};
use thinwire_protocol::RedactedPairingSecret;

/// whatsapp-rust client work for the link owner. No state of its own.
#[derive(Clone, Copy)]
pub(super) struct LiveBackend;

impl LinkBackend for LiveBackend {
    type Bot = BotHandle;

    async fn start(
        &self,
        _generation: u64,
        phone: Option<String>,
        callbacks: Callbacks,
        resume: bool,
    ) -> Result<Started<BotHandle>, StartError> {
        let path = whatsapp_device_store_path().map_err(|()| StartError::DataDir)?;
        // A resume never makes a store or a key: no file, no session.
        if resume && !path.is_file() {
            return Err(StartError::NotPaired);
        }
        let parent = path
            .parent()
            .map(std::path::Path::to_path_buf)
            .ok_or(StartError::DataDir)?;
        tokio::task::spawn_blocking(move || prepare_session_dir(&parent))
            .await
            .ok()
            .and_then(Result::ok)
            .ok_or(StartError::Store)?;
        let store_path = path.clone();
        let protected =
            tokio::task::spawn_blocking(move || super::cipher::protect_store(&store_path))
                .await
                .map_err(|_| StartError::Store)?;
        protected.map_err(|fail| match fail {
            super::cipher::ProtectFail::KeychainDenied => StartError::KeychainDenied {
                saved: path.is_file(),
            },
            super::cipher::ProtectFail::WrongKey => StartError::WrongKey,
            super::cipher::ProtectFail::NotMoved => StartError::MoveFailed,
            super::cipher::ProtectFail::NotSaved => {
                StartError::Reported(thinwire_protocol::whatsapp_key_not_saved().to_string())
            }
            super::cipher::ProtectFail::EncryptFailed => {
                StartError::Reported(thinwire_protocol::whatsapp_encrypt_failed().to_string())
            }
        })?;
        let db = path.to_str().ok_or(StartError::Store)?;
        let backend = super::cipher::open_store(db)
            .await
            .map_err(|()| StartError::Store)?;
        let _ = restrict_store_file(&path);
        // A store from a pairing that did not end has no phone number. A
        // resume of it would ask for a new QR code: stop before the client.
        let paired = {
            use whatsapp_rust::store::DeviceStore as _;
            backend
                .load()
                .await
                .ok()
                .flatten()
                .is_some_and(|device| device.pn.is_some())
        };
        if resume && !paired {
            return Err(StartError::NotPaired);
        }
        // The chat cache belongs to the paired device of this store. A new
        // pairing starts with an empty cache.
        let key = super::cipher::cache_key().map_err(|()| StartError::Store)?;
        let cache = Arc::new(super::cache::DiskCache::beside(&path, key));
        let saved = {
            let cache = Arc::clone(&cache);
            tokio::task::spawn_blocking(move || {
                if paired {
                    cache.load()
                } else {
                    cache.remove();
                    super::inbox::SavedInbox::default()
                }
            })
            .await
            .unwrap_or_default()
        };
        let bot = build_bot(backend, digits_only(phone), callbacks)
            .await
            .map_err(|()| StartError::Build)?;
        let client = bot.client();
        Ok(Started {
            bot: bot.spawn(),
            sender: Arc::new(LiveSender { client }),
            saved,
            cache: Some(cache),
        })
    }

    async fn stop(&self, bot: BotHandle) {
        note_shutdown(bot.shutdown().await);
    }

    async fn delete_store(&self) -> Result<(), ()> {
        let path = whatsapp_device_store_path()?;
        tokio::task::spawn_blocking(move || {
            // The chats and the audio files of a revoked device go with its
            // store.
            super::cache::remove_cache_files(&super::cache::cache_path(&path));
            if let Some(dir) = path.parent() {
                thinwire_protocol::voice_files::clear(&super::path::voice_dir_of(dir));
            }
            remove_device_store(&path)
        })
        .await
        .map_err(|_| ())?
        .map_err(|_| ())
    }

    async fn mark_revoked(&self) -> Result<(), ()> {
        let path = whatsapp_device_store_path()?;
        let marker = revoked_marker(&path);
        tokio::task::spawn_blocking(move || std::fs::write(marker, b""))
            .await
            .map_err(|_| ())?
            .map_err(|_| ())
    }

    async fn has_store(&self) -> bool {
        let Ok(path) = whatsapp_device_store_path() else {
            return false;
        };
        tokio::task::spawn_blocking(move || path.is_file())
            .await
            .unwrap_or(false)
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
    let mut builder = Bot::builder()
        .with_backend(backend)
        .with_device_props(device_props())
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

/// The name on the phone's Linked devices list. The library default is
/// "rust". WhatsApp reads it only when the device pairs, so a device that
/// linked before keeps the old name until it links again.
pub(crate) const DEVICE_NAME: &str = "thinwire";

fn device_props() -> DevicePropsOverride {
    DevicePropsOverride::new().with_os(DEVICE_NAME)
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
        Event::MuteUpdate(update) => LinkEvent::Mute {
            jid: update.jid.to_string(),
            mute: Mute::from_action(update.action.muted, update.action.mute_end_timestamp),
        },
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
                        sticker: sticker_of(&inbound.message),
                        voice: voice_of(&inbound.message),
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
                mute: Mute::from_history(conversation.mute_end_time),
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
        sticker: info.message.as_option().and_then(sticker_of),
        voice: info.message.as_option().and_then(voice_of),
    })
}

/// The sticker of a sticker message, when the app can draw it: a WebP or a
/// PNG image of [`MEDIA_MAX_BYTES`] or less. A Lottie sticker is not an
/// image, so its row keeps "[sticker]".
fn sticker_of(message: &wa::Message) -> Option<Sticker> {
    let sticker = message.get_base_message().sticker_message.as_option()?;
    if sticker.is_lottie == Some(true) {
        return None;
    }
    let drawable = sticker
        .mimetype
        .as_deref()
        .is_none_or(|mime| mime == "image/webp" || mime == "image/png");
    let fits = sticker
        .file_length
        .is_none_or(|length| usize::try_from(length).is_ok_and(|length| length <= MEDIA_MAX_BYTES));
    if !drawable || !fits {
        return None;
    }
    Some(Sticker {
        width: sticker.width.unwrap_or(0),
        height: sticker.height.unwrap_or(0),
        media: MediaRef::new(sticker.clone()),
    })
}

/// The audio of a voice note or an audio message, when it is not larger
/// than the helper keeps.
fn voice_of(message: &wa::Message) -> Option<Voice> {
    let audio = message.get_base_message().audio_message.as_option()?;
    let fits = audio.file_length.is_none_or(|length| {
        usize::try_from(length)
            .is_ok_and(|length| length <= thinwire_protocol::voice_files::VOICE_MAX_BYTES)
    });
    if !fits {
        return None;
    }
    Some(Voice {
        seconds: audio.seconds.unwrap_or(0),
        media: MediaRef::new(audio.clone()),
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
    } else if base
        .audio_message
        .as_option()
        .is_some_and(|audio| audio.ptt == Some(true))
    {
        "[voice message]"
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

/// Read the shutdown report. A device flush that failed, a message-secret
/// batch that did not persist, or a background run that did not finish is
/// logged as a fixed class. The report text can carry a path or a server
/// string, so it stays out of the log.
fn note_shutdown(report: BotShutdownReport) {
    if let Err(error) = &report.shutdown.device {
        tracing::warn!(
            error = store_error_class(error),
            "whatsapp shutdown did not save the device"
        );
    }
    let secrets = &report.shutdown.message_secrets;
    if secrets.failed_batches > 0 {
        tracing::warn!(
            failed_batches = secrets.failed_batches,
            error = secrets.first_error.as_deref().map(store_error_class),
            "whatsapp shutdown did not save message secrets"
        );
    }
    if let Some(outcome) = run_failure(&report.run) {
        tracing::warn!(outcome, "whatsapp background run did not finish cleanly");
    }
}

fn store_error_class(error: &StoreError) -> &'static str {
    match error {
        StoreError::Io(_) => "io",
        StoreError::Serialization(_) => "serialization",
        StoreError::Validation(_) => "validation",
        StoreError::InvalidMessageSecret(_) => "invalid message secret",
        StoreError::Connection(_) => "connection",
        StoreError::Database(_) => "database",
        StoreError::RetriesExhausted { .. } => "retries exhausted",
        StoreError::Migration(_) => "migration",
        StoreError::InvalidConfig(_) => "invalid config",
        StoreError::DeviceNotFound(_) => "device not found",
        _ => "store",
    }
}

/// `None` when the background run was observed to finish. An abort, a lost
/// task, and a run loop this handle did not own are failures. A session that
/// ended on its own (`Stopped`, reconnect disabled) is not.
fn run_failure(run: &BotRunOutcome) -> Option<&'static str> {
    match run {
        BotRunOutcome::Completed(
            RunCompletionReason::ShutdownRequested
            | RunCompletionReason::Stopped
            | RunCompletionReason::AutoReconnectDisabled { .. },
        ) => None,
        BotRunOutcome::Completed(RunCompletionReason::AlreadyRunning) => Some("already running"),
        BotRunOutcome::AbortRequested => Some("aborted"),
        BotRunOutcome::Unobserved => Some("unobserved"),
        BotRunOutcome::Completed(_) => Some("unexpected"),
        _ => Some("unexpected"),
    }
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
                .map(|sent| sent.message_id.to_string())
                .map_err(|error| classify_send_error(&error))
        })
    }

    fn send_media<'a>(
        &'a self,
        chat_jid: &'a str,
        file: &'a thinwire_protocol::OutgoingFile,
        caption: &'a str,
    ) -> SendFuture<'a> {
        Box::pin(async move {
            use whatsapp_rust::download::MediaType;
            use whatsapp_rust::waproto::buffa::MessageField;
            let jid: Jid = chat_jid.parse().map_err(|_| SendFailure::Rejected)?;
            // An error can hold the path. It stays out of the log.
            let size = tokio::fs::metadata(&file.path)
                .await
                .map_err(|_| SendFailure::Rejected)?
                .len();
            if size > thinwire_protocol::MAX_ATTACHMENT_BYTES {
                return Err(SendFailure::Rejected);
            }
            let data = tokio::fs::read(&file.path)
                .await
                .map_err(|_| SendFailure::Rejected)?;
            let kind = match file.kind {
                thinwire_protocol::MediaKind::Photo => MediaType::Image,
                thinwire_protocol::MediaKind::Video => MediaType::Video,
            };
            let upload = self
                .client
                .upload(data, kind, whatsapp_rust::UploadOptions::new())
                .await
                .map_err(|_| SendFailure::Network)?;
            let caption = (!caption.is_empty()).then(|| caption.to_string());
            let mut message = wa::Message::default();
            // The proto structs are non-exhaustive: set the fields one by one.
            match file.kind {
                thinwire_protocol::MediaKind::Photo => {
                    let mut image = wa::message::ImageMessage::default();
                    image.url = Some(upload.url);
                    image.mimetype = Some(file.mime().to_string());
                    image.caption = caption;
                    image.file_sha256 = Some(upload.file_sha256.to_vec());
                    image.file_length = Some(upload.file_length);
                    image.media_key = Some(upload.media_key.to_vec());
                    image.file_enc_sha256 = Some(upload.file_enc_sha256.to_vec());
                    image.direct_path = Some(upload.direct_path);
                    image.media_key_timestamp = Some(upload.media_key_timestamp);
                    message.image_message = MessageField::some(image);
                }
                thinwire_protocol::MediaKind::Video => {
                    let mut video = wa::message::VideoMessage::default();
                    video.url = Some(upload.url);
                    video.mimetype = Some(file.mime().to_string());
                    video.caption = caption;
                    video.file_sha256 = Some(upload.file_sha256.to_vec());
                    video.file_length = Some(upload.file_length);
                    video.media_key = Some(upload.media_key.to_vec());
                    video.file_enc_sha256 = Some(upload.file_enc_sha256.to_vec());
                    video.direct_path = Some(upload.direct_path);
                    video.media_key_timestamp = Some(upload.media_key_timestamp);
                    video.streaming_sidecar = upload.streaming_sidecar;
                    message.video_message = MessageField::some(video);
                }
            }
            self.client
                .send_message(jid, message)
                .await
                .map(|sent| sent.message_id.to_string())
                .map_err(|error| classify_send_error(&error))
        })
    }

    fn download<'a>(&'a self, media: &'a MediaRef) -> DownloadFuture<'a> {
        Box::pin(async move {
            // The error can name a host or a path. It stays out of the log.
            if let Some(sticker) = media.get::<wa::message::StickerMessage>() {
                let bytes = self
                    .client
                    .download(sticker, Default::default())
                    .await
                    .map_err(|_| ())?;
                return (bytes.len() <= MEDIA_MAX_BYTES).then_some(bytes).ok_or(());
            }
            let audio = media.get::<wa::message::AudioMessage>().ok_or(())?;
            let bytes = self
                .client
                .download(audio, Default::default())
                .await
                .map_err(|_| ())?;
            (bytes.len() <= thinwire_protocol::voice_files::VOICE_MAX_BYTES)
                .then_some(bytes)
                .ok_or(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_classes_drop_the_report_text() {
        let planted = "15551212 pair ABCD hello";
        let errors = [
            StoreError::Io(std::io::Error::other(planted)),
            StoreError::Validation(planted.into()),
            StoreError::InvalidConfig(planted.into()),
            StoreError::Database(Box::new(std::io::Error::other(planted))),
            StoreError::RetriesExhausted {
                op: planted.into(),
                attempts: 2,
                source: Box::new(std::io::Error::other(planted)),
            },
            StoreError::DeviceNotFound(7),
        ];
        for error in &errors {
            let class = store_error_class(error);
            assert!(!class.contains("1555"), "{class}");
            assert!(!class.contains("ABCD"), "{class}");
            assert!(!class.contains('7'), "{class}");
        }
        assert_eq!(
            store_error_class(&StoreError::Validation(planted.into())),
            "validation"
        );
        assert_eq!(
            store_error_class(&StoreError::DeviceNotFound(7)),
            "device not found"
        );

        let ended = BotRunOutcome::Completed(RunCompletionReason::AutoReconnectDisabled {
            connection: Some(whatsapp_rust::wacore::net::DisconnectReason::ReadError(
                planted.into(),
            )),
            connect_error: None,
            protocol_error: None,
        });
        assert_eq!(run_failure(&ended), None);
        assert_eq!(
            run_failure(&BotRunOutcome::Completed(
                RunCompletionReason::ShutdownRequested
            )),
            None
        );
        assert_eq!(
            run_failure(&BotRunOutcome::Completed(RunCompletionReason::Stopped)),
            None
        );
        assert_eq!(
            run_failure(&BotRunOutcome::Completed(
                RunCompletionReason::AlreadyRunning
            )),
            Some("already running")
        );
        assert_eq!(run_failure(&BotRunOutcome::AbortRequested), Some("aborted"));
        assert_eq!(run_failure(&BotRunOutcome::Unobserved), Some("unobserved"));
    }

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

    /// The phone lists this device as "thinwire", not as the library
    /// default "rust".
    #[test]
    fn the_linked_device_is_named_thinwire() {
        assert_eq!(device_props().os.as_deref(), Some("thinwire"));
    }

    /// A WebP sticker can be drawn. A Lottie sticker and a large file
    /// cannot: their rows keep "[sticker]".
    #[test]
    fn only_a_small_image_sticker_downloads() {
        let with = |edit: fn(&mut wa::message::StickerMessage)| {
            let mut sticker = wa::message::StickerMessage::default();
            sticker.mimetype = Some("image/webp".into());
            sticker.width = Some(512);
            sticker.height = Some(512);
            sticker.file_length = Some(40_000);
            edit(&mut sticker);
            let mut message = wa::Message::default();
            message.sticker_message = whatsapp_rust::waproto::buffa::MessageField::some(sticker);
            message
        };
        let webp = sticker_of(&with(|_| {})).expect("a webp sticker");
        assert_eq!((webp.width, webp.height), (512, 512));
        assert_eq!(body_of(&with(|_| {})).as_deref(), Some("[sticker]"));
        assert!(sticker_of(&with(|sticker| sticker.is_lottie = Some(true))).is_none());
        assert!(sticker_of(&with(|sticker| sticker.file_length = Some(10_000_000))).is_none());
        assert!(
            sticker_of(&with(
                |sticker| sticker.mimetype = Some("application/was".into())
            ))
            .is_none()
        );
        assert!(sticker_of(&wa::Message::text("hi")).is_none());
    }

    #[test]
    fn body_uses_text_caption_or_media_label() {
        assert_eq!(body_of(&wa::Message::text("  hi ")).as_deref(), Some("hi"));
        let mut image = wa::Message::default();
        image.image_message =
            whatsapp_rust::waproto::buffa::MessageField::some(wa::message::ImageMessage::default());
        assert_eq!(body_of(&image).as_deref(), Some("[photo]"));
        assert_eq!(body_of(&wa::Message::default()), None);
    }
}
