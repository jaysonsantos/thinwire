// SPDX-License-Identifier: AGPL-3.0-only
//! Live backend of the WhatsApp link. Compiled only with `whatsapp-web`.
//!
//! The link owner ([`super::link`]) calls [`LiveBackend`] one step at a time.
//! This module holds no lifecycle state. `Bot` runs on a tokio task; the egui
//! thread never calls into this module. QR payloads and pair codes are
//! events, not command fields, and are not logged.

use std::sync::Arc;

use whatsapp_rust::bot::{Bot, BotHandle, BotRunOutcome, BotShutdownReport};
use whatsapp_rust::download::{DownloadLimits, DownloadParams, Downloadable, MediaType};
use whatsapp_rust::pair_code::PairCodeOptions;
use whatsapp_rust::send::SendError;
use whatsapp_rust::store::SqliteStore;
use whatsapp_rust::store::error::StoreError;
use whatsapp_rust::types::events::{Event, EventKind};
use whatsapp_rust::wacore::proto_helpers::{MessageBuilderExt, MessageExt};
use whatsapp_rust::wacore::store::DevicePropsOverride;
use whatsapp_rust::waproto::whatsapp as wa;
use whatsapp_rust::{Client, ClientError, Jid, RunCompletionReason};

use super::inbox::TargetRef;
use super::inbox::{
    HistoryChat, ME, MEDIA_MAX_BYTES, MediaClass, MediaRef, Mute, PHOTO_LABEL,
    STICKER_DOWNLOAD_MAX, Sticker, THUMBNAIL_MAX_BYTES, Video, Voice, WaContact, WaMessage,
    WaReaction,
};
use super::link::{Callbacks, LinkBackend, StartError, Started};
use super::path::{
    prepare_session_dir, remove_device_store, restrict_store_file, revoked_marker,
    whatsapp_device_store_path,
};
use super::session::{
    ActionFuture, DownloadFuture, HistoryAsk, LinkEvent, NamesFuture, PhoneFuture, SendFailure,
    SendFuture, SubjectFuture, VideoFuture, WhatsAppSender,
};
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

    fn log_out(&self, bot: &BotHandle) -> impl Future<Output = bool> + Send {
        // The client is taken here, so the future holds no borrow of the
        // handle. `logout` sends the "remove companion device" request and
        // then shuts the client down. It does not delete the store.
        let client = bot.client();
        async move {
            matches!(
                client.logout().await.deregistration,
                whatsapp_rust::DeregistrationOutcome::Confirmed
            )
        }
    }

    async fn delete_store(&self) -> Result<(), ()> {
        let path = whatsapp_device_store_path()?;
        tokio::task::spawn_blocking(move || {
            // The chats, the audio files, and the video files of a revoked
            // device go with its store.
            super::cache::remove_cache_files(&super::cache::cache_path(&path));
            if let Some(dir) = path.parent() {
                thinwire_protocol::voice_files::clear(&super::path::voice_dir_of(dir));
                thinwire_protocol::voice_files::clear(&super::path::video_dir_of(dir));
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
    EventKind::MuteUpdate,
    EventKind::ContactUpdate,
    EventKind::ContactRemoved,
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
                if let Some(reactions) = map_reactions(&event) {
                    callbacks.send(LinkEvent::Reactions(reactions));
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
        Event::HistorySync(sync) => {
            let request_id = sync.peer_data_request_session_id().map(str::to_string);
            let mut event = map_history(sync.get()?);
            if let LinkEvent::History {
                request_id: slot, ..
            } = &mut event
            {
                *slot = request_id;
            }
            event
        }
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
                        sender_alt: info
                            .source
                            .sender_alt
                            .as_ref()
                            .map(|jid| jid.to_non_ad().to_string()),
                        body: body_of(&inbound.message)?,
                        timestamp: info.timestamp.timestamp(),
                        sticker: picture_of(&inbound.message),
                        voice: voice_of(&inbound.message),
                        video: video_of(&inbound.message),
                        reactions: Vec::new(),
                        reply_to: reply_of(&inbound.message),
                        mentions: mentions_of(&inbound.message),
                    })
                })
                .collect(),
        ),
        Event::ContactUpdate(update) => {
            LinkEvent::Contacts(vec![contact_of(&update.jid, &update.action)])
        }
        Event::ContactRemoved(removed) => LinkEvent::Contacts(vec![WaContact {
            jid: removed.jid.to_non_ad().to_string(),
            name: None,
            lid: None,
            phone: None,
        }]),
        _ => return None,
    })
}

/// An address-book contact: its full name, else its first name, and its
/// LID and phone JID when the action has them.
fn contact_of(jid: &Jid, action: &wa::sync_action_value::ContactAction) -> WaContact {
    let jid_of = |value: &Option<String>| {
        value
            .as_deref()
            .and_then(|value| value.parse::<Jid>().ok())
            .map(|jid| jid.to_non_ad().to_string())
    };
    WaContact {
        jid: jid.to_non_ad().to_string(),
        name: action
            .full_name
            .as_deref()
            .and_then(non_empty)
            .or_else(|| action.first_name.as_deref().and_then(non_empty)),
        lid: jid_of(&action.lid_jid),
        phone: jid_of(&action.pn_jid),
    }
}

fn map_history(sync: &wa::HistorySync) -> LinkEvent {
    let mut ended = Vec::new();
    let chats = sync
        .conversations
        .iter()
        .map(|conversation| {
            let jid = conversation.id.clone();
            if no_older_messages(conversation) {
                ended.push(jid.clone());
            }
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
    LinkEvent::History {
        chats,
        push_names,
        request_id: None,
        ended,
    }
}

/// The phone says this chat has no older page this device can read.
///
/// `COMPLETE_AND_NO_MORE_MESSAGE_REMAIN_ON_PRIMARY` means the primary has
/// nothing older. `COMPLETE_ON_DEMAND_SYNC_WITH_MORE_MSG_ON_PRIMARY_BUT_NO_ACCESS`
/// means the primary still has older messages, and this linked device is
/// not allowed to read them. Another request cannot return those messages,
/// so the chat ends here.
fn no_older_messages(conversation: &wa::Conversation) -> bool {
    use whatsapp_rust::waproto::whatsapp::conversation::EndOfHistoryTransferType;
    matches!(
        conversation.end_of_history_transfer_type,
        Some(
            EndOfHistoryTransferType::COMPLETE_AND_NO_MORE_MESSAGE_REMAIN_ON_PRIMARY
                | EndOfHistoryTransferType::COMPLETE_ON_DEMAND_SYNC_WITH_MORE_MSG_ON_PRIMARY_BUT_NO_ACCESS,
        )
    )
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
        sender_alt: None,
        body: body_of(info.message.as_option()?)?,
        timestamp: info
            .message_timestamp
            .and_then(|ts| i64::try_from(ts).ok())
            .unwrap_or(0),
        sticker: info.message.as_option().and_then(picture_of),
        voice: info.message.as_option().and_then(voice_of),
        video: info.message.as_option().and_then(video_of),
        reactions: history_reactions(chat_jid, info),
        reply_to: info.message.as_option().and_then(reply_of),
        mentions: info
            .message
            .as_option()
            .map(mentions_of)
            .unwrap_or_default(),
    })
}

/// The id of the message that `message` replies to.
fn reply_of(message: &wa::Message) -> Option<String> {
    message
        .context_info()
        .and_then(|context| context.stanza_id.clone())
        .filter(|id| !id.is_empty())
}

/// The JIDs that `message` mentions. An empty or repeated JID is left out.
fn mentions_of(message: &wa::Message) -> Vec<String> {
    let mut jids: Vec<String> = Vec::new();
    if let Some(context) = message.context_info() {
        for jid in &context.mentioned_jid {
            let jid = jid.trim();
            if !jid.is_empty() && !jids.iter().any(|known| known == jid) {
                jids.push(jid.to_owned());
            }
        }
    }
    jids
}

/// The reactions that a history row carries.
fn history_reactions(chat_jid: &str, info: &wa::WebMessageInfo) -> Vec<(String, String)> {
    let mut reactions = Vec::new();
    for reaction in &info.reactions {
        let Some(key) = reaction.key.as_option() else {
            continue;
        };
        let reactor = if key.from_me.unwrap_or(false) {
            ME.to_owned()
        } else if let Some(participant) = key.participant.clone() {
            participant
        } else {
            chat_jid.to_owned()
        };
        let emoji = reaction.text.as_deref().unwrap_or_default();
        super::inbox::set_reaction(&mut reactions, &reactor, emoji);
    }
    reactions
}

/// The live reactions of a message batch. Each reaction message names the
/// message it reacts to. An empty text removes the reaction.
fn map_reactions(event: &Event) -> Option<Vec<WaReaction>> {
    let Event::Messages(batch) = event else {
        return None;
    };
    let reactions: Vec<WaReaction> = batch
        .iter()
        .filter_map(|inbound| {
            let reaction = inbound.message.reaction_message.as_option()?;
            let target_id = reaction.key.as_option()?.id.clone()?;
            let info = &inbound.info;
            Some(WaReaction {
                chat_jid: info.source.chat.to_string(),
                target_id,
                reactor: if info.source.is_from_me {
                    ME.to_owned()
                } else {
                    info.source.sender.to_non_ad().to_string()
                },
                emoji: reaction.text.clone().unwrap_or_default(),
            })
        })
        .collect();
    (!reactions.is_empty()).then_some(reactions)
}

/// The download reference of an encrypted media file. `None` when the
/// message has no path, no key of 32 bytes, or no hashes of 32 bytes: such
/// a file does not download.
fn media_ref(class: MediaClass, media: &dyn Downloadable) -> Option<MediaRef> {
    const HASH: usize = 32;
    let direct_path = media.direct_path().filter(|path| !path.is_empty())?;
    let media_key = media.media_key().filter(|key| key.len() == HASH)?;
    let file_sha256 = media.file_sha256().filter(|hash| hash.len() == HASH)?;
    let file_enc_sha256 = media.file_enc_sha256().filter(|hash| hash.len() == HASH)?;
    Some(MediaRef {
        class,
        direct_path: direct_path.to_owned(),
        media_key: media_key.to_vec(),
        file_sha256: file_sha256.to_vec(),
        file_enc_sha256: file_enc_sha256.to_vec(),
        file_length: media.file_length().unwrap_or(0),
    })
}

/// `true` when the file is not larger than `max` bytes. A message with no
/// length passes: the download has its own limit.
fn fits(length: Option<u64>, max: u64) -> bool {
    length.is_none_or(|length| length <= max)
}

/// The sticker of a sticker message, when the app can draw it: a WebP or a
/// PNG image of [`STICKER_DOWNLOAD_MAX`] or less. An animated sticker shows
/// its first picture. A Lottie sticker is not an image, so its row keeps
/// "[sticker]".
fn sticker_of(message: &wa::Message) -> Option<Sticker> {
    let sticker = message.get_base_message().sticker_message.as_option()?;
    if sticker.is_lottie == Some(true) {
        return None;
    }
    let drawable = sticker
        .mimetype
        .as_deref()
        .is_none_or(|mime| mime == "image/webp" || mime == "image/png");
    if !drawable || !fits(sticker.file_length, STICKER_DOWNLOAD_MAX) {
        return None;
    }
    Some(Sticker {
        width: sticker.width.unwrap_or(0),
        height: sticker.height.unwrap_or(0),
        media: media_ref(MediaClass::Sticker, sticker)?,
        photo: false,
    })
}

/// The sticker or the photo of a message, when the app can draw it.
fn picture_of(message: &wa::Message) -> Option<Sticker> {
    sticker_of(message).or_else(|| photo_of(message))
}

/// The photo of a photo message, when it is not larger than the helper
/// downloads. A view-once photo does not download: its row keeps the label.
fn photo_of(message: &wa::Message) -> Option<Sticker> {
    if message.is_view_once() {
        return None;
    }
    let image = message.get_base_message().image_message.as_option()?;
    if !fits(
        image.file_length,
        thinwire_protocol::photo_resize::PHOTO_DOWNLOAD_MAX as u64,
    ) {
        return None;
    }
    Some(Sticker {
        width: image.width.unwrap_or(0),
        height: image.height.unwrap_or(0),
        media: media_ref(MediaClass::Image, image)?,
        photo: true,
    })
}

/// The audio of a voice note or an audio message, when it is not larger
/// than the helper keeps. A view-once voice note does not download.
fn voice_of(message: &wa::Message) -> Option<Voice> {
    if message.is_view_once() {
        return None;
    }
    let audio = message.get_base_message().audio_message.as_option()?;
    if !fits(
        audio.file_length,
        thinwire_protocol::voice_files::VOICE_MAX_BYTES as u64,
    ) {
        return None;
    }
    Some(Voice {
        seconds: audio.seconds.unwrap_or(0),
        media: media_ref(MediaClass::Audio, audio)?,
    })
}

/// The video of a video message, of a round video message (`ptv_message`),
/// or of a GIF, when it is not larger than the helper keeps. Only the round
/// video message is a note. A view-once video does not download: its row
/// keeps the label.
fn video_of(message: &wa::Message) -> Option<Video> {
    if message.is_view_once() {
        return None;
    }
    let base = message.get_base_message();
    let (video, note) = match base.video_message.as_option() {
        Some(video) => (video, false),
        None => (base.ptv_message.as_option()?, true),
    };
    if video.view_once == Some(true)
        || !fits(
            video.file_length,
            thinwire_protocol::voice_files::VIDEO_MAX_BYTES as u64,
        )
    {
        return None;
    }
    Some(Video {
        seconds: video.seconds.unwrap_or(0),
        width: video.width.unwrap_or(0),
        height: video.height.unwrap_or(0),
        media: media_ref(MediaClass::Video, video)?,
        note,
        thumbnail: video
            .jpeg_thumbnail
            .clone()
            .filter(|bytes| !bytes.is_empty() && bytes.len() <= THUMBNAIL_MAX_BYTES),
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
        PHOTO_LABEL
    } else if base
        .video_message
        .as_option()
        .is_some_and(|video| video.gif_playback == Some(true))
    {
        "[GIF]"
    } else if base.video_message.is_set() {
        "[video]"
    } else if base.ptv_message.is_set() {
        "[video message]"
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
    } else if base.sticker_message.is_set() || base.lottie_sticker_message.is_set() {
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

/// The largest profile picture the helper reads. A preview is far less.
const PICTURE_MAX_BYTES: usize = 1024 * 1024;

impl LiveSender {
    /// The address of a cached message. A group message of another person
    /// needs its sender.
    fn reference<'a>(
        chat: &'a Jid,
        sender: Option<&'a Jid>,
        target: &TargetRef,
    ) -> Result<whatsapp_rust::MessageRef<'a>, ()> {
        let id = whatsapp_rust::MessageId::new(&target.id).map_err(|_| ())?;
        whatsapp_rust::MessageRef::new(chat, id, sender, target.from_me).map_err(|_| ())
    }
}

impl WhatsAppSender for LiveSender {
    fn send_reply<'a>(
        &'a self,
        chat_jid: &'a str,
        body: &'a str,
        quote: &'a TargetRef,
    ) -> SendFuture<'a> {
        Box::pin(async move {
            let jid: Jid = chat_jid.parse().map_err(|_| SendFailure::Rejected)?;
            // The quote names who wrote the message: this account, the sender
            // in a group, or the other person of a direct chat.
            let author = if quote.from_me {
                self.client.pn().map(|own| own.to_non_ad().to_string())
            } else {
                quote.sender_jid.clone()
            }
            .unwrap_or_else(|| chat_jid.to_string());
            let context = whatsapp_rust::wacore::proto_helpers::build_quote_context(
                quote.id.clone(),
                author,
                &wa::Message::text(quote.body.clone()),
            );
            self.client
                .send_message(jid, wa::Message::text_with_context(body, context))
                .await
                .map(|sent| sent.message_id.to_string())
                .map_err(|error| classify_send_error(&error))
        })
    }

    fn react<'a>(
        &'a self,
        chat_jid: &'a str,
        target: &'a TargetRef,
        emoji: &'a str,
    ) -> ActionFuture<'a> {
        Box::pin(async move {
            let chat: Jid = chat_jid.parse().map_err(|_| ())?;
            let sender: Option<Jid> = target
                .sender_jid
                .as_deref()
                .and_then(|jid| jid.parse().ok());
            let reference = Self::reference(&chat, sender.as_ref(), target)?;
            self.client
                .send_reaction(&reference, emoji)
                .await
                .map(|_| ())
                .map_err(|_| ())
        })
    }

    fn revoke<'a>(&'a self, chat_jid: &'a str, target: &'a TargetRef) -> ActionFuture<'a> {
        Box::pin(async move {
            let chat: Jid = chat_jid.parse().map_err(|_| ())?;
            let sender: Option<Jid> = target
                .sender_jid
                .as_deref()
                .and_then(|jid| jid.parse().ok());
            let reference = Self::reference(&chat, sender.as_ref(), target)?;
            self.client
                .revoke_message(&reference)
                .await
                .map(|_| ())
                .map_err(|_| ())
        })
    }

    fn delete_for_me<'a>(&'a self, chat_jid: &'a str, target: &'a TargetRef) -> ActionFuture<'a> {
        Box::pin(async move {
            let chat: Jid = chat_jid.parse().map_err(|_| ())?;
            let sender: Option<Jid> = target
                .sender_jid
                .as_deref()
                .and_then(|jid| jid.parse().ok());
            let reference = Self::reference(&chat, sender.as_ref(), target)?;
            self.client
                .chat_actions()
                .delete_message_for_me(&reference, true, Some(target.timestamp))
                .await
                .map_err(|_| ())
        })
    }

    fn group_names(&self) -> NamesFuture<'_> {
        Box::pin(async move {
            // The error can name a group. It stays out of the log.
            let groups = self
                .client
                .groups()
                .list_participating()
                .await
                .map_err(|_| ())?;
            Ok(groups
                .into_iter()
                .filter_map(|group| {
                    let subject = group.subject?.trim().to_string();
                    (!subject.is_empty()).then(|| (group.id.to_string(), subject))
                })
                .collect())
        })
    }

    fn group_subject<'a>(&'a self, jid: &'a str) -> SubjectFuture<'a> {
        Box::pin(async move {
            let jid: Jid = jid.parse().map_err(|_| ())?;
            let meta = self
                .client
                .groups()
                .fetch_metadata(&jid)
                .await
                .map_err(|_| ())?;
            let subject = meta.subject.unwrap_or_default();
            let subject = subject.trim();
            Ok((!subject.is_empty()).then(|| subject.to_string()))
        })
    }

    fn phone_of<'a>(&'a self, lid: &'a str) -> PhoneFuture<'a> {
        Box::pin(async move {
            let jid: Jid = lid.parse().ok()?;
            let entry = self.client.get_lid_pn_entry(&jid).await.ok()??;
            let phone = entry.phone_number.trim();
            (!phone.is_empty()).then(|| format!("{phone}@s.whatsapp.net"))
        })
    }

    fn allocate_history_id(&self) -> Option<String> {
        Some(self.client.generate_message_id().as_str().to_string())
    }

    fn request_history<'a>(&'a self, ask: &'a HistoryAsk<'a>) -> ActionFuture<'a> {
        Box::pin(async move {
            let chat: Jid = ask.chat_jid.parse().map_err(|_| ())?;
            let request_id = whatsapp_rust::MessageId::new(ask.request_id).map_err(|_| ())?;
            self.client
                .fetch_message_history_with_id(
                    &chat,
                    ask.oldest_id,
                    ask.oldest_from_me,
                    ask.oldest_timestamp_ms,
                    ask.count,
                    &request_id,
                )
                .await
                .map(|_| ())
                .map_err(|_| ())
        })
    }

    fn profile_picture<'a>(&'a self, jid: &'a str) -> DownloadFuture<'a> {
        use whatsapp_rust::wacore::iq::contacts::ProfilePictureLookup;
        use whatsapp_rust::{ProfilePictureRequest, ProfilePictureTarget, ProfilePictureType};
        Box::pin(async move {
            let jid: Jid = jid.parse().map_err(|_| ())?;
            let target = if jid.to_string().ends_with("@g.us") {
                ProfilePictureTarget::Group(&jid)
            } else {
                ProfilePictureTarget::Contact(&jid)
            };
            let lookup = self
                .client
                .pictures()
                .lookup(ProfilePictureRequest::new(
                    target,
                    ProfilePictureType::Preview,
                ))
                .await
                .map_err(|_| ())?;
            let ProfilePictureLookup::Found(picture) = lookup else {
                return Err(());
            };
            // The URL holds a signed token. It stays out of the log.
            let response = self
                .client
                .http_client()
                .execute(whatsapp_rust::wacore::net::HttpRequest::get(picture.url))
                .await
                .map_err(|_| ())?;
            if !(200..300).contains(&response.status_code)
                || response.body.is_empty()
                || response.body.len() > PICTURE_MAX_BYTES
            {
                return Err(());
            }
            Ok(response.body)
        })
    }

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
            let max = match media.class {
                MediaClass::Sticker => STICKER_DOWNLOAD_MAX,
                MediaClass::Image => thinwire_protocol::photo_resize::PHOTO_DOWNLOAD_MAX as u64,
                MediaClass::Audio => thinwire_protocol::voice_files::VOICE_MAX_BYTES as u64,
                // A video goes to a file: `download_video`.
                MediaClass::Video => return Err(()),
            };
            let params = download_params(media)?;
            let bytes = self
                .client
                .download(&params, download_limits(max))
                .await
                .map_err(|_| ())?;
            match media.class {
                // An animated sticker becomes its first picture, and a
                // large one becomes smaller, so the image fits one line.
                MediaClass::Sticker => tokio::task::spawn_blocking(move || {
                    thinwire_protocol::photo_resize::sticker_from_bytes(&bytes)
                })
                .await
                .map_err(|_| ())?
                .filter(|bytes| bytes.len() <= MEDIA_MAX_BYTES)
                .ok_or(()),
                // A photo crosses the pipe as an image that fits one line.
                MediaClass::Image => tokio::task::spawn_blocking(move || {
                    thinwire_protocol::photo_resize::photo_from_bytes(&bytes)
                })
                .await
                .map_err(|_| ())?
                .ok_or(())?
                .sizes
                .into_iter()
                .find_map(|size| size.bytes)
                .map(|bytes| bytes.as_slice().to_vec())
                .ok_or(()),
                MediaClass::Audio | MediaClass::Video => Ok(bytes),
            }
        })
    }

    fn download_video<'a>(
        &'a self,
        media: &'a MediaRef,
        dir: &'a std::path::Path,
        file_id: i32,
    ) -> VideoFuture<'a> {
        Box::pin(async move {
            use thinwire_protocol::voice_files::{VIDEO_MAX_BYTES, VideoPart};
            if media.class != MediaClass::Video {
                return Err(());
            }
            let params = download_params(media)?;
            let folder = dir.to_path_buf();
            let (file, part) =
                tokio::task::spawn_blocking(move || VideoPart::begin(&folder, file_id))
                    .await
                    .map_err(|_| ())?
                    .map_err(|_| ())?;
            // The client decrypts into the file and checks both hashes. A
            // failed download drops `part`, and the part file goes.
            let file = self
                .client
                .download_to_writer(&params, file, download_limits(VIDEO_MAX_BYTES as u64))
                .await
                .map_err(|_| ())?;
            tokio::task::spawn_blocking(move || part.finish(file))
                .await
                .map_err(|_| ())?
                .map_err(|_| ())
        })
    }
}

/// The largest padding and checksum that WhatsApp adds to an encrypted
/// media file: one cipher block and the 10 bytes of the MAC.
const MEDIA_CIPHER_OVERHEAD: u64 = 16 + 10;

/// The limits of one download: `max` plain bytes, and the encrypted bytes
/// of such a file.
const fn download_limits(max: u64) -> DownloadLimits {
    DownloadLimits::new(max + MEDIA_CIPHER_OVERHEAD, max)
}

/// The values that the client needs for the download of `media`.
fn download_params(media: &MediaRef) -> Result<DownloadParams, ()> {
    let kind = match media.class {
        MediaClass::Sticker => MediaType::Sticker,
        MediaClass::Image => MediaType::Image,
        MediaClass::Audio => MediaType::Audio,
        MediaClass::Video => MediaType::Video,
    };
    DownloadParams::encrypted(
        media.direct_path.as_str(),
        &media.media_key,
        &media.file_sha256,
        &media.file_enc_sha256,
        media.file_length,
        kind,
    )
    .map_err(|_| ())
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

    /// The fields that every media message needs for a download.
    macro_rules! downloadable {
        ($media:expr) => {{
            $media.direct_path = Some("/v/t62/fixture.enc".into());
            $media.media_key = Some(vec![7; 32]);
            $media.file_sha256 = Some(vec![8; 32]);
            $media.file_enc_sha256 = Some(vec![9; 32]);
        }};
    }

    fn wrapped(inner: wa::Message) -> wa::message::FutureProofMessage {
        let mut wrapper = wa::message::FutureProofMessage::default();
        wrapper.message = whatsapp_rust::waproto::buffa::MessageField::some(inner);
        wrapper
    }

    fn video_with(edit: fn(&mut wa::message::VideoMessage)) -> wa::message::VideoMessage {
        let mut video = wa::message::VideoMessage::default();
        downloadable!(video);
        video.mimetype = Some("video/mp4".into());
        video.seconds = Some(12);
        video.width = Some(640);
        video.height = Some(360);
        video.file_length = Some(2_000_000);
        video.jpeg_thumbnail = Some(vec![0xFF, 0xD8, 0xFF, 0xD9]);
        edit(&mut video);
        video
    }

    fn video_message(video: wa::message::VideoMessage) -> wa::Message {
        let mut message = wa::Message::default();
        message.video_message = whatsapp_rust::waproto::buffa::MessageField::some(video);
        message
    }

    /// A video message has a download reference, its length, its size, and
    /// its thumbnail. A GIF is a normal clip with its own label. A caption
    /// is the body.
    #[test]
    fn a_video_and_a_gif_are_clips() {
        let plain = video_message(video_with(|_| {}));
        assert_eq!(body_of(&plain).as_deref(), Some("[video]"));
        let video = video_of(&plain).expect("a video");
        assert_eq!((video.seconds, video.width, video.height), (12, 640, 360));
        assert!(!video.note);
        assert_eq!(
            video.thumbnail.as_deref(),
            Some(&[0xFF, 0xD8, 0xFF, 0xD9][..])
        );
        assert_eq!(video.media.class, MediaClass::Video);
        assert_eq!(video.media.file_length, 2_000_000);
        assert_eq!(video.media.media_key, vec![7; 32]);
        assert!(download_params(&video.media).is_ok());
        assert!(picture_of(&plain).is_none() && voice_of(&plain).is_none());

        let gif = video_message(video_with(|video| video.gif_playback = Some(true)));
        assert_eq!(body_of(&gif).as_deref(), Some("[GIF]"));
        assert!(!video_of(&gif).expect("a clip").note);

        let caption = video_message(video_with(|video| video.caption = Some("look".into())));
        assert_eq!(body_of(&caption).as_deref(), Some("look"));
        assert!(video_of(&caption).is_some());

        // A message in a chat with a timer has a wrapper.
        let mut timed = wa::Message::default();
        timed.ephemeral_message =
            whatsapp_rust::waproto::buffa::MessageField::some(wrapped(plain.clone()));
        assert!(video_of(&timed).is_some());

        // A thumbnail that is too large is left out. The video stays.
        let huge = video_message(video_with(|video| {
            video.jpeg_thumbnail = Some(vec![0; 100_000])
        }));
        assert!(video_of(&huge).expect("a video").thumbnail.is_none());
    }

    /// A round video message is a note. Before this change its row did not
    /// show at all.
    #[test]
    fn a_round_video_message_is_a_note() {
        let mut message = wa::Message::default();
        message.ptv_message = whatsapp_rust::waproto::buffa::MessageField::some(video_with(|_| {}));
        assert_eq!(body_of(&message).as_deref(), Some("[video message]"));
        let video = video_of(&message).expect("a video message");
        assert!(video.note);
        assert_eq!(video.seconds, 12);
    }

    /// A video that is too large, or that has no key, keeps its label only.
    #[test]
    fn a_video_that_cannot_download_keeps_its_label() {
        let large = video_message(video_with(|video| {
            video.file_length = Some(thinwire_protocol::voice_files::VIDEO_MAX_BYTES as u64 + 1);
        }));
        assert!(video_of(&large).is_none());
        assert_eq!(body_of(&large).as_deref(), Some("[video]"));
        for edit in [
            (|video| video.media_key = None) as fn(&mut wa::message::VideoMessage),
            |video| video.media_key = Some(vec![7; 5]),
            |video| video.direct_path = Some(String::new()),
            |video| video.file_enc_sha256 = None,
            |video| video.file_sha256 = Some(vec![1]),
        ] {
            assert!(video_of(&video_message(video_with(edit))).is_none());
        }
        assert!(video_of(&wa::Message::text("hi")).is_none());
    }

    /// View-once media does not download and does not show. The row keeps
    /// its label.
    #[test]
    fn view_once_media_keeps_its_label() {
        let flagged = video_message(video_with(|video| video.view_once = Some(true)));
        assert!(video_of(&flagged).is_none());
        assert_eq!(body_of(&flagged).as_deref(), Some("[video]"));

        let mut image = wa::message::ImageMessage::default();
        downloadable!(image);
        let mut photo = wa::Message::default();
        photo.image_message = whatsapp_rust::waproto::buffa::MessageField::some(image.clone());
        assert!(photo_of(&photo).is_some(), "a normal photo downloads");

        let mut audio = wa::message::AudioMessage::default();
        downloadable!(audio);
        audio.ptt = Some(true);
        let mut voice = wa::Message::default();
        voice.audio_message = whatsapp_rust::waproto::buffa::MessageField::some(audio);
        assert!(voice_of(&voice).is_some(), "a normal voice note downloads");

        for (inner, label) in [
            (video_message(video_with(|_| {})), "[video]"),
            (photo, "[photo]"),
            (voice, "[voice message]"),
        ] {
            let mut old = wa::Message::default();
            old.view_once_message =
                whatsapp_rust::waproto::buffa::MessageField::some(wrapped(inner.clone()));
            let mut v2 = wa::Message::default();
            v2.view_once_message_v2 =
                whatsapp_rust::waproto::buffa::MessageField::some(wrapped(inner.clone()));
            let mut extension = wa::Message::default();
            extension.view_once_message_v2_extension =
                whatsapp_rust::waproto::buffa::MessageField::some(wrapped(inner));
            for once in [old, v2, extension] {
                assert!(video_of(&once).is_none(), "{label}");
                assert!(picture_of(&once).is_none(), "{label}");
                assert!(voice_of(&once).is_none(), "{label}");
                assert_eq!(body_of(&once).as_deref(), Some(label));
            }
        }
        image.view_once = Some(true);
        let mut inline = wa::Message::default();
        inline.image_message = whatsapp_rust::waproto::buffa::MessageField::some(image);
        assert!(photo_of(&inline).is_none());
        assert_eq!(body_of(&inline).as_deref(), Some("[photo]"));
    }

    /// Each media class has the key name of its WhatsApp file type.
    #[test]
    fn a_media_reference_gives_the_download_values() {
        let mut sticker = wa::message::StickerMessage::default();
        downloadable!(sticker);
        sticker.file_length = Some(40_000);
        let reference = media_ref(MediaClass::Sticker, &sticker).expect("a reference");
        let params = download_params(&reference).expect("values");
        assert_eq!(params.direct_path(), Some("/v/t62/fixture.enc"));
        assert_eq!(params.media_key(), Some(&[7; 32][..]));
        assert_eq!(params.file_length(), Some(40_000));
        for (class, kind) in [
            (MediaClass::Sticker, MediaType::Sticker),
            (MediaClass::Image, MediaType::Image),
            (MediaClass::Audio, MediaType::Audio),
            (MediaClass::Video, MediaType::Video),
        ] {
            let reference = MediaRef {
                class,
                ..reference.clone()
            };
            assert_eq!(
                download_params(&reference).expect("values").app_info(),
                kind
            );
        }
        let debug = format!("{reference:?}");
        assert!(
            !debug.contains("fixture") && !debug.contains('7'),
            "{debug}"
        );
    }

    /// A WebP sticker can be drawn, also an animated one. A Lottie sticker
    /// and a large file cannot: their rows keep "[sticker]".
    #[test]
    fn only_a_small_image_sticker_downloads() {
        let with = |edit: fn(&mut wa::message::StickerMessage)| {
            let mut sticker = wa::message::StickerMessage::default();
            downloadable!(sticker);
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
        assert_eq!(webp.media.class, MediaClass::Sticker);
        assert!(sticker_of(&with(|sticker| sticker.is_animated = Some(true))).is_some());
        // An animated sticker can be larger than one line of the pipe: the
        // helper sends its first picture.
        assert!(sticker_of(&with(|sticker| sticker.file_length = Some(900_000))).is_some());
        assert!(sticker_of(&with(|sticker| sticker.is_lottie = Some(true))).is_none());
        assert!(sticker_of(&with(|sticker| sticker.file_length = Some(10_000_000))).is_none());
        assert!(sticker_of(&with(|sticker| sticker.media_key = None)).is_none());
        // A Lottie sticker in its wrapper is a row with the label. Before
        // this change the row did not show at all.
        let mut lottie = wa::Message::default();
        lottie.lottie_sticker_message =
            whatsapp_rust::waproto::buffa::MessageField::some(wrapped(with(|sticker| {
                sticker.is_lottie = Some(true)
            })));
        assert_eq!(body_of(&lottie).as_deref(), Some("[sticker]"));
        assert!(picture_of(&lottie).is_none());
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
