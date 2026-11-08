//! Maps between the adapter contract and the wire types of `thinwire-ipc`.
//!
//! The protocol of an event comes from the helper the line came from, never
//! from the line body. A helper thus cannot send an event for another
//! protocol.

use thinwire_ipc::{
    SecretText, WireAccountState, WireArrival, WireAudio, WireCommand, WireConversation,
    WireDelivery, WireEvent, WireFile, WireHistoryCursor, WireHistorySkip, WireMediaKind,
    WireMention, WireMessage, WirePhoto, WireProtocol, WireStatus, WireStoreAccess,
};

use crate::media::{
    AudioPath, ImageBytes, MediaKind, MessageAudio, MessageLink, MessagePhoto, OutgoingFile,
    PhotoSize,
};

use crate::ProtocolKind;
use crate::adapter::{
    AccountState, AdapterCommand, AdapterEvent, AdapterStatus, Arrival, ChatMessage, Conversation,
    Delivery, HistorySkipRun, HistoryStoreCursor, MAX_HISTORY_CURSOR_RUNS, ProtocolId,
    RedactedPairingSecret, StoreAccessReason,
};

/// The wire name of a protocol that runs in a helper. `None` for a protocol
/// that runs in the app.
#[must_use]
pub(crate) const fn wire_protocol(protocol: ProtocolId) -> Option<WireProtocol> {
    match protocol.kind() {
        ProtocolKind::WhatsApp => Some(WireProtocol::WhatsApp),
        ProtocolKind::Signal => Some(WireProtocol::Signal),
        ProtocolKind::Telegram | ProtocolKind::Discord | ProtocolKind::Slack => None,
    }
}

// region: commands

/// The wire copy of a command of `protocol`. `None` for a command that no
/// helper takes, or one of another protocol. `phone` gives the number for a
/// pair code: it is read only for a pairing start.
pub(crate) fn command_to_wire(
    protocol: ProtocolId,
    command: &AdapterCommand,
    phone: impl FnOnce() -> Option<String>,
) -> Option<WireCommand> {
    if command.protocol() != protocol {
        return None;
    }
    Some(match command {
        AdapterCommand::Connect { .. } => WireCommand::Connect,
        AdapterCommand::Disconnect { .. } => WireCommand::Disconnect,
        AdapterCommand::LoadChats { .. } => WireCommand::LoadChats,
        AdapterCommand::OpenChat {
            conversation_id, ..
        } => WireCommand::OpenChat {
            conversation_id: conversation_id.clone(),
        },
        AdapterCommand::LoadOlderMessages {
            conversation_id,
            before_message_id,
            store_cursor,
            ..
        } => WireCommand::LoadOlderMessages {
            conversation_id: conversation_id.clone(),
            before_message_id: before_message_id.clone(),
            store_cursor: *store_cursor,
        },
        AdapterCommand::SendText {
            conversation_id,
            body,
            request,
            reply_to,
            ..
        } => WireCommand::SendText {
            conversation_id: conversation_id.clone(),
            body: body.clone(),
            request: *request,
            reply_to: reply_to.clone(),
        },
        AdapterCommand::DeleteMessage {
            conversation_id,
            message_id,
            for_everyone,
            ..
        } => WireCommand::DeleteMessage {
            conversation_id: conversation_id.clone(),
            message_id: message_id.clone(),
            for_everyone: *for_everyone,
        },
        AdapterCommand::React {
            conversation_id,
            message_id,
            emoji,
            add,
            ..
        } => WireCommand::React {
            conversation_id: conversation_id.clone(),
            message_id: message_id.clone(),
            emoji: emoji.clone(),
            add: *add,
        },
        AdapterCommand::SendMedia {
            conversation_id,
            files,
            caption,
            request,
            topic: None,
            ..
        } => WireCommand::SendMedia {
            conversation_id: conversation_id.clone(),
            files: files
                .iter()
                .map(|file| WireFile {
                    path: file.path.to_string_lossy().into_owned(),
                    kind: match file.kind {
                        MediaKind::Photo => WireMediaKind::Photo,
                        MediaKind::Video => WireMediaKind::Video,
                    },
                })
                .collect(),
            caption: caption.clone(),
            request: *request,
        },
        AdapterCommand::ResendMessage {
            conversation_id,
            message_id,
            request,
            ..
        } => WireCommand::ResendMessage {
            conversation_id: conversation_id.clone(),
            message_id: message_id.clone(),
            request: *request,
        },
        AdapterCommand::ViewChat {
            conversation_id, ..
        } => WireCommand::ViewChat {
            conversation_id: conversation_id.clone(),
        },
        AdapterCommand::WhatsAppAcknowledgeRisk | AdapterCommand::SignalAcknowledgeNotice => {
            WireCommand::AcknowledgeGate
        }
        AdapterCommand::WhatsAppBeginLink { generation }
        | AdapterCommand::SignalBeginLink { generation } => WireCommand::BeginLink {
            generation: *generation,
            phone: phone().map(SecretText::new),
        },
        AdapterCommand::WhatsAppCancelLink | AdapterCommand::SignalCancelLink => {
            WireCommand::CancelLink
        }
        AdapterCommand::RetryStoreKey { .. } => WireCommand::RetryStoreKey,
        AdapterCommand::DownloadVoice { file_id, .. } => WireCommand::DownloadVoice {
            file_id: *file_id,
        },
        // The app answers these itself. They never cross the pipe.
        // Topic commands are Telegram-only. A helper never sees them.
        AdapterCommand::Shutdown { .. }
        | AdapterCommand::RestartHelper { .. }
        | AdapterCommand::ConnectDiscord { .. }
        | AdapterCommand::TelegramAuth { .. }
        | AdapterCommand::OpenTopic { .. }
        | AdapterCommand::LoadOlderTopic { .. }
        | AdapterCommand::SendTopicText { .. }
        | AdapterCommand::SendMedia { topic: Some(_), .. }
        // The host answers these. A routed command has no `ToAccount`.
        | AdapterCommand::AddAccount { .. }
        | AdapterCommand::RemoveAccount { .. }
        | AdapterCommand::ToAccount { .. }
        | AdapterCommand::ViewTopic { .. }
        | AdapterCommand::DownloadFile { .. }
        // A helper sends no video yet (ADR 0015).
        | AdapterCommand::DownloadVideo { .. }
        | AdapterCommand::DownloadDocument { .. }
        // A helper has no edit yet (#519, #520).
        | AdapterCommand::EditMessage { .. }
        // A helper has no forward yet (#533, #534).
        | AdapterCommand::ForwardMessages { .. }
        // A helper sends no typing state yet (#524, #525).
        | AdapterCommand::SetTyping { .. } => return None,
    })
}

/// The adapter command of a wire command, in the helper of `protocol`, and
/// the phone of a pairing start. `None`: this protocol has no such command.
pub(crate) fn command_from_wire(
    protocol: ProtocolId,
    command: WireCommand,
) -> Option<(AdapterCommand, Option<SecretText>)> {
    let command = match command {
        WireCommand::Connect => AdapterCommand::Connect { protocol },
        WireCommand::Disconnect => AdapterCommand::Disconnect { protocol },
        WireCommand::LoadChats => AdapterCommand::LoadChats { protocol },
        WireCommand::OpenChat { conversation_id } => AdapterCommand::OpenChat {
            protocol,
            conversation_id,
        },
        WireCommand::LoadOlderMessages {
            conversation_id,
            before_message_id,
            store_cursor,
        } => AdapterCommand::LoadOlderMessages {
            protocol,
            conversation_id,
            before_message_id,
            store_cursor,
        },
        WireCommand::SendText {
            conversation_id,
            body,
            request,
            reply_to,
        } => AdapterCommand::SendText {
            protocol,
            conversation_id,
            body,
            request,
            reply_to,
        },
        WireCommand::DeleteMessage {
            conversation_id,
            message_id,
            for_everyone,
        } => AdapterCommand::DeleteMessage {
            protocol,
            conversation_id,
            message_id,
            for_everyone,
        },
        WireCommand::React {
            conversation_id,
            message_id,
            emoji,
            add,
        } => AdapterCommand::React {
            protocol,
            conversation_id,
            message_id,
            emoji,
            add,
        },
        WireCommand::SendMedia {
            conversation_id,
            files,
            caption,
            request,
        } => AdapterCommand::SendMedia {
            protocol,
            conversation_id,
            topic: None,
            files: files
                .into_iter()
                .map(|file| OutgoingFile {
                    path: file.path.into(),
                    kind: match file.kind {
                        WireMediaKind::Photo => MediaKind::Photo,
                        WireMediaKind::Video => MediaKind::Video,
                    },
                })
                .collect(),
            caption,
            request,
        },
        WireCommand::ResendMessage {
            conversation_id,
            message_id,
            request,
        } => AdapterCommand::ResendMessage {
            protocol,
            conversation_id,
            message_id,
            request,
        },
        WireCommand::ViewChat { conversation_id } => AdapterCommand::ViewChat {
            protocol,
            conversation_id,
        },
        WireCommand::AcknowledgeGate => match protocol {
            ProtocolId::WhatsApp => AdapterCommand::WhatsAppAcknowledgeRisk,
            ProtocolId::Signal => AdapterCommand::SignalAcknowledgeNotice,
            _ => return None,
        },
        WireCommand::BeginLink { generation, phone } => {
            let command = match protocol {
                ProtocolId::WhatsApp => AdapterCommand::WhatsAppBeginLink { generation },
                ProtocolId::Signal => AdapterCommand::SignalBeginLink { generation },
                _ => return None,
            };
            return Some((command, phone));
        }
        WireCommand::CancelLink => match protocol {
            ProtocolId::WhatsApp => AdapterCommand::WhatsAppCancelLink,
            ProtocolId::Signal => AdapterCommand::SignalCancelLink,
            _ => return None,
        },
        WireCommand::RetryStoreKey => AdapterCommand::RetryStoreKey { protocol },
        WireCommand::DownloadVoice { file_id } => {
            AdapterCommand::DownloadVoice { protocol, file_id }
        }
    };
    Some((command, None))
}

// endregion: commands

// region: events

/// The wire copy of an event of `protocol`. `None` for an event that does
/// not cross the pipe (a Telegram login event, `Stopped`, a helper state),
/// and for an event of another protocol.
pub(crate) fn event_to_wire(protocol: ProtocolId, event: AdapterEvent) -> Option<WireEvent> {
    let own = |seen: ProtocolId| seen == protocol;
    Some(match event {
        AdapterEvent::Status {
            protocol: seen,
            status,
            detail,
        } if own(seen) => WireEvent::Status {
            status: status_to_wire(status),
            detail,
        },
        AdapterEvent::StoreAccess {
            protocol: seen,
            reason,
        } if own(seen) => {
            // A failed move crosses as the status sentence. Version 2 has no
            // third `StoreAccess` reason.
            let reason = match reason {
                StoreAccessReason::KeychainDenied => WireStoreAccess::KeychainDenied,
                StoreAccessReason::WrongKey => WireStoreAccess::WrongKey,
                StoreAccessReason::MoveFailed => return None,
            };
            WireEvent::StoreAccess { reason }
        }
        AdapterEvent::Account {
            protocol: seen,
            state,
        } if own(seen) => WireEvent::Account {
            state: account_to_wire(state),
        },
        AdapterEvent::AccountEnded { protocol: seen } if own(seen) => WireEvent::AccountEnded,
        AdapterEvent::ConversationUpsert { conversation } if own(conversation.protocol) => {
            WireEvent::ConversationUpsert {
                conversation: conversation_to_wire(conversation),
            }
        }
        AdapterEvent::ConversationRemoved { protocol: seen, id } if own(seen) => {
            WireEvent::ConversationRemoved { id }
        }
        AdapterEvent::MessageReceived { message } if own(message.protocol) => {
            WireEvent::MessageReceived {
                message: message_to_wire(message),
            }
        }
        AdapterEvent::MessageReplaced {
            protocol: seen,
            conversation_id,
            old_id,
            message,
        } if own(seen) && own(message.protocol) => WireEvent::MessageReplaced {
            conversation_id,
            old_id,
            message: message_to_wire(message),
        },
        AdapterEvent::MessageBody {
            protocol: seen,
            conversation_id,
            message_id,
            body,
        } if own(seen) => WireEvent::MessageBody {
            conversation_id,
            message_id,
            body,
        },
        AdapterEvent::MessagesRemoved {
            protocol: seen,
            conversation_id,
            message_ids,
        } if own(seen) => WireEvent::MessagesRemoved {
            conversation_id,
            message_ids,
        },
        AdapterEvent::MessageDelivery {
            protocol: seen,
            conversation_id,
            message_id,
            delivery,
        } if own(seen) => WireEvent::MessageDelivery {
            conversation_id,
            message_id,
            delivery: delivery_to_wire(delivery),
        },
        AdapterEvent::SendAccepted {
            protocol: seen,
            conversation_id,
            request,
        } if own(seen) => WireEvent::SendAccepted {
            conversation_id,
            request,
        },
        AdapterEvent::SendRejected {
            protocol: seen,
            conversation_id,
            request,
        } if own(seen) => WireEvent::SendRejected {
            conversation_id,
            request,
        },
        AdapterEvent::CommandFailed {
            protocol: seen,
            conversation_id,
            detail,
        } if own(seen) => WireEvent::CommandFailed {
            conversation_id,
            detail,
        },
        AdapterEvent::Notice {
            protocol: seen,
            text,
        } if own(seen) => WireEvent::Notice { text },
        AdapterEvent::VoiceReady {
            protocol: seen,
            file_id,
            path,
        } if own(seen) => WireEvent::VoiceReady {
            file_id,
            path: path.as_path().to_string_lossy().into_owned(),
        },
        AdapterEvent::VoiceFailed {
            protocol: seen,
            file_id,
            ..
        } if own(seen) => WireEvent::VoiceFailed { file_id },
        // A picture that does not fit one line does not cross.
        AdapterEvent::ChatAvatar {
            protocol: seen,
            conversation_id,
            bytes,
        } if own(seen) && !bytes.is_empty() && bytes.as_slice().len() <= WirePhoto::MAX_BYTES => {
            WireEvent::Avatar {
                conversation_id,
                photo: WirePhoto {
                    width: 0,
                    height: 0,
                    bytes: bytes.as_slice().to_vec(),
                },
            }
        }
        AdapterEvent::MessageReactions {
            protocol: seen,
            conversation_id,
            message_id,
            reactions,
        } if own(seen) => WireEvent::Reactions {
            conversation_id,
            message_id,
            reactions: reactions_to_wire(&reactions),
        },
        AdapterEvent::ChatListLoaded { protocol: seen } if own(seen) => WireEvent::ChatListLoaded,
        AdapterEvent::HistoryLoaded {
            protocol: seen,
            conversation_id,
            store_cursor,
        } if own(seen) => WireEvent::HistoryLoaded {
            conversation_id,
            store_cursor: store_cursor.map(cursor_to_wire),
        },
        AdapterEvent::OlderHistoryLoaded {
            protocol: seen,
            conversation_id,
            before_message_id,
            more,
            note,
            topic: _,
            store_cursor,
        } if own(seen) => WireEvent::OlderHistoryLoaded {
            conversation_id,
            before_message_id,
            more,
            note,
            store_cursor: store_cursor.map(cursor_to_wire),
        },
        AdapterEvent::WhatsAppQr { code, generation } if own(ProtocolId::WhatsApp) => {
            WireEvent::Qr {
                code: SecretText::new(code.reveal()),
                generation,
            }
        }
        AdapterEvent::SignalQr { code, generation } if own(ProtocolId::Signal) => WireEvent::Qr {
            code: SecretText::new(code.reveal()),
            generation,
        },
        AdapterEvent::WhatsAppPairCode { code, generation } if own(ProtocolId::WhatsApp) => {
            WireEvent::PairCode {
                code: SecretText::new(code.reveal()),
                generation,
            }
        }
        _ => return None,
    })
}

/// The adapter event of a wire event from the helper of `protocol`. `None`:
/// this protocol has no such event.
pub(crate) fn event_from_wire(protocol: ProtocolId, event: WireEvent) -> Option<AdapterEvent> {
    Some(match event {
        WireEvent::Status { status, detail } => AdapterEvent::Status {
            protocol,
            status: status_from_wire(status),
            detail,
        },
        WireEvent::StoreAccess { reason } => AdapterEvent::StoreAccess {
            protocol,
            reason: match reason {
                WireStoreAccess::KeychainDenied => StoreAccessReason::KeychainDenied,
                WireStoreAccess::WrongKey => StoreAccessReason::WrongKey,
            },
        },
        WireEvent::Account { state } => AdapterEvent::Account {
            protocol,
            state: account_from_wire(state),
        },
        WireEvent::AccountEnded => AdapterEvent::AccountEnded { protocol },
        WireEvent::ConversationUpsert { conversation } => AdapterEvent::ConversationUpsert {
            conversation: conversation_from_wire(protocol, conversation),
        },
        WireEvent::ConversationRemoved { id } => AdapterEvent::ConversationRemoved { protocol, id },
        WireEvent::MessageReceived { message } => AdapterEvent::MessageReceived {
            message: message_from_wire(protocol, message),
        },
        WireEvent::MessageReplaced {
            conversation_id,
            old_id,
            message,
        } => AdapterEvent::MessageReplaced {
            protocol,
            conversation_id,
            old_id,
            message: message_from_wire(protocol, message),
        },
        WireEvent::MessageBody {
            conversation_id,
            message_id,
            body,
        } => AdapterEvent::MessageBody {
            protocol,
            conversation_id,
            message_id,
            body,
        },
        WireEvent::MessagesRemoved {
            conversation_id,
            message_ids,
        } => AdapterEvent::MessagesRemoved {
            protocol,
            conversation_id,
            message_ids,
        },
        WireEvent::MessageDelivery {
            conversation_id,
            message_id,
            delivery,
        } => AdapterEvent::MessageDelivery {
            protocol,
            conversation_id,
            message_id,
            delivery: delivery_from_wire(delivery),
        },
        WireEvent::SendAccepted {
            conversation_id,
            request,
        } => AdapterEvent::SendAccepted {
            protocol,
            conversation_id,
            request,
        },
        WireEvent::SendRejected {
            conversation_id,
            request,
        } => AdapterEvent::SendRejected {
            protocol,
            conversation_id,
            request,
        },
        WireEvent::CommandFailed {
            conversation_id,
            detail,
        } => AdapterEvent::CommandFailed {
            protocol,
            conversation_id,
            detail,
        },
        WireEvent::Notice { text } => AdapterEvent::Notice { protocol, text },
        WireEvent::ChatListLoaded => AdapterEvent::ChatListLoaded { protocol },
        WireEvent::HistoryLoaded {
            conversation_id,
            store_cursor,
        } => AdapterEvent::HistoryLoaded {
            protocol,
            conversation_id,
            store_cursor: store_cursor.map(cursor_from_wire),
        },
        WireEvent::OlderHistoryLoaded {
            conversation_id,
            before_message_id,
            more,
            note,
            store_cursor,
        } => AdapterEvent::OlderHistoryLoaded {
            protocol,
            conversation_id,
            before_message_id,
            more,
            note,
            topic: None,
            store_cursor: store_cursor.map(cursor_from_wire),
        },
        WireEvent::Qr { code, generation } => {
            let code = RedactedPairingSecret::new(code.expose());
            match protocol {
                ProtocolId::WhatsApp => AdapterEvent::WhatsAppQr { code, generation },
                ProtocolId::Signal => AdapterEvent::SignalQr { code, generation },
                _ => return None,
            }
        }
        WireEvent::PairCode { code, generation } => match protocol {
            ProtocolId::WhatsApp => AdapterEvent::WhatsAppPairCode {
                code: RedactedPairingSecret::new(code.expose()),
                generation,
            },
            _ => return None,
        },
        // The app opens only an absolute path. Anything else is a failed
        // download, so the player does not wait for it.
        WireEvent::VoiceReady { file_id, path } => {
            match AudioPath::new(path).filter(|path| path.as_path().is_absolute()) {
                Some(path) => AdapterEvent::VoiceReady {
                    protocol,
                    file_id,
                    path,
                },
                None => AdapterEvent::VoiceFailed {
                    protocol,
                    file_id,
                    stop: crate::media::VoiceStop::Unplayable,
                },
            }
        }
        WireEvent::VoiceFailed { file_id } => AdapterEvent::VoiceFailed {
            protocol,
            file_id,
            stop: crate::media::VoiceStop::Unplayable,
        },
        WireEvent::Avatar {
            conversation_id,
            photo,
        } => {
            if photo.bytes.is_empty() {
                return None;
            }
            AdapterEvent::ChatAvatar {
                protocol,
                conversation_id,
                bytes: ImageBytes::new(photo.bytes),
            }
        }
        WireEvent::Reactions {
            conversation_id,
            message_id,
            reactions,
        } => AdapterEvent::MessageReactions {
            protocol,
            conversation_id,
            message_id,
            reactions: reactions_from_wire(reactions),
        },
    })
}

fn reactions_to_wire(reactions: &[crate::Reaction]) -> Vec<thinwire_ipc::WireReaction> {
    reactions
        .iter()
        .map(|reaction| thinwire_ipc::WireReaction {
            emoji: reaction.emoji.clone(),
            count: reaction.count,
            mine: reaction.mine,
        })
        .collect()
}

/// The reactions of a wire line. An empty emoji or a count of 0 is dropped.
fn reactions_from_wire(reactions: Vec<thinwire_ipc::WireReaction>) -> Vec<crate::Reaction> {
    reactions
        .into_iter()
        .filter(|reaction| !reaction.emoji.is_empty() && reaction.count > 0)
        .map(|reaction| crate::Reaction {
            emoji: reaction.emoji,
            count: reaction.count,
            mine: reaction.mine,
        })
        .collect()
}

// endregion: events

// region: values

fn cursor_to_wire(cursor: HistoryStoreCursor) -> WireHistoryCursor {
    WireHistoryCursor {
        before_ms: cursor.before_ms,
        skipped: cursor.skipped,
        resume: cursor.resume,
        runs: cursor
            .runs
            .into_iter()
            .map(|run| WireHistorySkip {
                after_id: run.after_id,
                skipped: run.skipped,
            })
            .collect(),
    }
}

fn cursor_from_wire(cursor: WireHistoryCursor) -> HistoryStoreCursor {
    HistoryStoreCursor {
        before_ms: cursor.before_ms,
        skipped: cursor.skipped,
        resume: cursor.resume,
        runs: bounded_skip_runs(cursor.runs, cursor.skipped),
    }
}

/// Keep at most [`MAX_HISTORY_CURSOR_RUNS`] runs, and only when their counts
/// add up to `skipped`. A longer list is cut there. A list that still does
/// not add up is dropped. `skipped` stays, so the core draws one line for
/// the total at the oldest edge of the page.
fn bounded_skip_runs(runs: Vec<WireHistorySkip>, skipped: u32) -> Vec<HistorySkipRun> {
    if runs.is_empty() {
        return Vec::new();
    }
    let runs: Vec<HistorySkipRun> = runs
        .into_iter()
        .filter(|run| run.skipped > 0)
        .take(MAX_HISTORY_CURSOR_RUNS)
        .map(|run| HistorySkipRun {
            after_id: run.after_id,
            skipped: run.skipped,
        })
        .collect();
    let sum = runs
        .iter()
        .fold(0u32, |sum, run| sum.saturating_add(run.skipped));
    if sum != skipped {
        return Vec::new();
    }
    runs
}

const fn status_to_wire(status: AdapterStatus) -> WireStatus {
    match status {
        AdapterStatus::Stubbed => WireStatus::Stubbed,
        AdapterStatus::Connecting => WireStatus::Connecting,
        AdapterStatus::Ready => WireStatus::Ready,
        AdapterStatus::Refused => WireStatus::Refused,
        AdapterStatus::Error => WireStatus::Error,
    }
}

const fn status_from_wire(status: WireStatus) -> AdapterStatus {
    match status {
        WireStatus::Stubbed => AdapterStatus::Stubbed,
        WireStatus::Connecting => AdapterStatus::Connecting,
        WireStatus::Ready => AdapterStatus::Ready,
        WireStatus::Refused => AdapterStatus::Refused,
        WireStatus::Error => AdapterStatus::Error,
    }
}

const fn account_to_wire(state: AccountState) -> WireAccountState {
    match state {
        AccountState::Unlinked => WireAccountState::Unlinked,
        AccountState::Linking => WireAccountState::Linking,
        AccountState::Linked => WireAccountState::Linked,
    }
}

const fn account_from_wire(state: WireAccountState) -> AccountState {
    match state {
        WireAccountState::Unlinked => AccountState::Unlinked,
        WireAccountState::Linking => AccountState::Linking,
        WireAccountState::Linked => AccountState::Linked,
    }
}

const fn delivery_to_wire(delivery: Delivery) -> WireDelivery {
    match delivery {
        Delivery::Sent | Delivery::Read => WireDelivery::Sent,
        Delivery::Pending => WireDelivery::Pending,
        Delivery::Failed => WireDelivery::Failed,
    }
}

const fn delivery_from_wire(delivery: WireDelivery) -> Delivery {
    match delivery {
        WireDelivery::Sent => Delivery::Sent,
        WireDelivery::Pending => Delivery::Pending,
        WireDelivery::Failed => Delivery::Failed,
    }
}

const fn arrival_to_wire(arrival: Arrival) -> WireArrival {
    match arrival {
        Arrival::History => WireArrival::History,
        Arrival::Live => WireArrival::Live,
    }
}

const fn arrival_from_wire(arrival: WireArrival) -> Arrival {
    match arrival {
        WireArrival::History => Arrival::History,
        WireArrival::Live => Arrival::Live,
    }
}

fn conversation_to_wire(conversation: Conversation) -> WireConversation {
    WireConversation {
        id: conversation.id,
        title: conversation.title,
        participant: conversation.participant,
        preview: conversation.preview,
        unread: conversation.unread,
        order: conversation.order,
        last_at: conversation.last_at,
        is_group: conversation.is_group,
        writable: conversation.writable,
        placeholder: conversation.placeholder,
        muted: conversation.muted,
    }
}

fn conversation_from_wire(protocol: ProtocolId, conversation: WireConversation) -> Conversation {
    Conversation {
        protocol,
        id: conversation.id,
        title: conversation.title,
        participant: conversation.participant,
        preview: conversation.preview,
        unread: conversation.unread,
        order: conversation.order,
        last_at: conversation.last_at,
        is_group: conversation.is_group,
        writable: conversation.writable,
        placeholder: conversation.placeholder,
        muted: conversation.muted,
        has_topics: false,
        avatar: Default::default(),
    }
}

fn message_to_wire(message: ChatMessage) -> Box<WireMessage> {
    Box::new(WireMessage {
        conversation_id: message.conversation_id,
        id: message.id,
        sender: message.sender,
        body: message.body,
        outbound: message.outbound,
        delivery: delivery_to_wire(message.delivery),
        sent_at: message.sent_at,
        arrival: arrival_to_wire(message.arrival),
        photo: message.photo.as_ref().and_then(photo_to_wire),
        audio: message.audio.as_ref().and_then(audio_to_wire),
        reactions: reactions_to_wire(&message.reactions),
        reply_to: message.reply_to.clone(),
        mentions: mentions_to_wire(&message.links),
    })
}

/// The mentions of a message. A web link does not cross: the app finds
/// the web links in the body itself (`message_from_wire`).
fn mentions_to_wire(links: &[MessageLink]) -> Vec<WireMention> {
    links
        .iter()
        .filter(|link| link.is_mention())
        .filter_map(|link| {
            Some(WireMention {
                start: u32::try_from(link.start).ok()?,
                end: u32::try_from(link.end).ok()?,
                conversation_id: link.chat().map(str::to_owned),
            })
        })
        .collect()
}

/// The mentions of a wire message. The app checks each range against the
/// body, and drops a chat of another protocol.
fn mentions_from_wire(
    protocol: ProtocolId,
    body: &str,
    mentions: Vec<WireMention>,
) -> Vec<MessageLink> {
    let prefix = format!("{}:", protocol.kind().key());
    let ranges = mentions
        .into_iter()
        .filter_map(|mention| {
            let start = usize::try_from(mention.start).ok()?;
            let end = usize::try_from(mention.end).ok()?;
            let chat = mention
                .conversation_id
                .filter(|chat| chat.starts_with(&prefix) && chat.len() > prefix.len());
            Some((start, end, chat))
        })
        .collect();
    crate::media::mention_links(body, ranges)
}

/// Audio that the helper can download. A file that is already local does
/// not cross: its path is a path of the helper.
fn audio_to_wire(audio: &MessageAudio) -> Option<WireAudio> {
    (audio.file_id() > 0).then(|| WireAudio {
        duration_secs: audio.duration_secs(),
        file_id: audio.file_id(),
    })
}

/// The first size that holds its bytes. A photo with no bytes, or with too
/// many for one line, does not cross: the row shows the body.
fn photo_to_wire(photo: &MessagePhoto) -> Option<WirePhoto> {
    let size = photo.sizes.iter().find(|size| size.bytes.is_some())?;
    let bytes = size.bytes.as_ref()?.as_slice();
    if bytes.is_empty() || bytes.len() > WirePhoto::MAX_BYTES {
        return None;
    }
    Some(WirePhoto {
        width: size.width,
        height: size.height,
        bytes: bytes.to_vec(),
    })
}

fn photo_from_wire(photo: WirePhoto) -> Option<MessagePhoto> {
    if photo.bytes.is_empty() {
        return None;
    }
    Some(MessagePhoto {
        width: photo.width,
        height: photo.height,
        mini: None,
        sizes: vec![PhotoSize {
            width: photo.width,
            height: photo.height,
            file_id: None,
            path: None,
            bytes: Some(ImageBytes::new(photo.bytes)),
        }],
        has_spoiler: false,
        is_secret: false,
        failed: false,
    })
}

fn message_from_wire(protocol: ProtocolId, message: Box<WireMessage>) -> ChatMessage {
    let message = *message;
    let mentions = mentions_from_wire(protocol, &message.body, message.mentions);
    // The helper sends no web link ranges. The app finds the addresses in
    // the body, so a link in a WhatsApp or Signal message opens.
    let links = crate::media::with_web_links(&message.body, mentions);
    ChatMessage {
        protocol,
        conversation_id: message.conversation_id,
        id: message.id,
        sender: message.sender,
        body: message.body,
        outbound: message.outbound,
        delivery: delivery_from_wire(message.delivery),
        sent_at: message.sent_at,
        arrival: arrival_from_wire(message.arrival),
        topic: None,
        links,
        photo: message.photo.and_then(photo_from_wire),
        audio: message
            .audio
            .and_then(|audio| MessageAudio::new(audio.duration_secs, audio.file_id, None)),
        video: None,
        file: None,
        reactions: reactions_from_wire(message.reactions),
        reply_to: message.reply_to,
        edited: false,
    }
}

// endregion: values

#[cfg(test)]
mod tests {
    use super::*;

    const CHAT: &str = "whatsapp:111@s.whatsapp.net";

    /// A mention crosses with its range and chat. A web link stays out.
    /// The app drops a bad range and a chat of another protocol.
    #[test]
    fn a_mention_crosses_and_the_app_checks_it() {
        let mut sent = message(ProtocolId::WhatsApp);
        sent.body = "@Ana and @Bo".into();
        sent.links = vec![
            MessageLink::mention(0, 4, Some(CHAT.into())),
            MessageLink::mention(9, 12, None),
            MessageLink::web(5, 8, "https://a.test"),
        ];
        let wire = message_to_wire(sent.clone());
        assert_eq!(wire.mentions.len(), 2);
        let back = message_from_wire(ProtocolId::WhatsApp, wire.clone());
        assert_eq!(back.links, sent.links[..2].to_vec());

        let mut bad = wire;
        bad.mentions = vec![
            WireMention {
                start: 0,
                end: 4,
                conversation_id: Some("telegram:1".into()),
            },
            WireMention {
                start: 2,
                end: 6,
                conversation_id: None,
            },
            WireMention {
                start: 5,
                end: 8,
                conversation_id: None,
            },
            WireMention {
                start: 9,
                end: 40,
                conversation_id: None,
            },
        ];
        let back = message_from_wire(ProtocolId::WhatsApp, bad);
        assert_eq!(
            back.links,
            vec![MessageLink::mention(0, 4, None)],
            "an overlap, a range with no @, and a range past the body are dropped"
        );
    }

    /// A helper sends a body with a bare address and no link range. The
    /// app finds the address, so a click opens it. A mention keeps its range.
    #[test]
    fn the_app_finds_the_web_links_of_a_helper_message() {
        let mut sent = message(ProtocolId::Signal);
        sent.body = "@Ana see https://youtu.be/abc123.".into();
        sent.links = vec![MessageLink::mention(0, 4, None)];
        let back = message_from_wire(ProtocolId::Signal, message_to_wire(sent.clone()));
        let start = sent.body.find("https").expect("address");
        assert_eq!(
            back.links,
            vec![
                MessageLink::mention(0, 4, None),
                MessageLink::web(start, sent.body.len() - 1, "https://youtu.be/abc123"),
            ]
        );
    }

    #[test]
    fn a_read_receipt_crosses_the_pipe_as_sent() {
        assert_eq!(delivery_to_wire(Delivery::Read), WireDelivery::Sent);
    }

    fn message(protocol: ProtocolId) -> ChatMessage {
        ChatMessage {
            protocol,
            conversation_id: CHAT.into(),
            id: "m1".into(),
            sender: "Ana".into(),
            body: "hello".into(),
            outbound: true,
            delivery: Delivery::Pending,
            sent_at: 1_790_000_000,
            arrival: Arrival::Live,
            topic: None,
            links: Vec::new(),
            photo: None,
            audio: None,
            video: None,
            file: None,
            reactions: Vec::new(),
            reply_to: None,
            edited: false,
        }
    }

    fn conversation(protocol: ProtocolId) -> Conversation {
        Conversation {
            protocol,
            id: CHAT.into(),
            title: "Ana".into(),
            participant: "Ana".into(),
            preview: "hello".into(),
            unread: 2,
            order: 7,
            last_at: 1_790_000_000,
            is_group: false,
            writable: true,
            placeholder: false,
            muted: true,
            has_topics: false,
            avatar: Default::default(),
        }
    }

    /// The audio of a message crosses as its handle and its duration: no
    /// path and no bytes. A download command and both answers cross too.
    /// A path that is not absolute is a failed download in the app.
    /// The ready path is absolute on this platform. A Unix `/tmp` string
    /// is not absolute on Windows.
    #[test]
    fn audio_crosses_as_a_handle_and_a_relative_path_fails() {
        let mut voice = message(ProtocolId::Signal);
        voice.audio = MessageAudio::new(4, 9, AudioPath::new("/tmp/helper-only.audio".to_string()));
        let wire = message_to_wire(voice);
        assert_eq!(
            wire.audio,
            Some(WireAudio {
                duration_secs: 4,
                file_id: 9
            })
        );
        let back = message_from_wire(ProtocolId::Signal, wire);
        let audio = back.audio.expect("audio");
        assert_eq!((audio.duration_secs(), audio.file_id()), (4, 9));
        assert!(
            audio.file().is_none(),
            "the helper path stays in the helper"
        );

        let command = AdapterCommand::DownloadVoice {
            protocol: ProtocolId::Signal,
            file_id: 9,
        };
        let wire = command_to_wire(ProtocolId::Signal, &command, || None).expect("crosses");
        assert_eq!(wire, WireCommand::DownloadVoice { file_id: 9 });
        let (back, _) = command_from_wire(ProtocolId::Signal, wire).expect("back");
        assert_eq!(back, command);

        let absolute = std::env::temp_dir().join("9.audio");
        assert!(absolute.is_absolute());
        let ready = event_to_wire(
            ProtocolId::Signal,
            AdapterEvent::VoiceReady {
                protocol: ProtocolId::Signal,
                file_id: 9,
                path: AudioPath::new(absolute.to_string_lossy()).expect("path"),
            },
        )
        .expect("crosses");
        assert!(matches!(
            event_from_wire(ProtocolId::Signal, ready),
            Some(AdapterEvent::VoiceReady { file_id: 9, .. })
        ));
        let relative = WireEvent::VoiceReady {
            file_id: 9,
            path: "voice/9.audio".into(),
        };
        assert!(matches!(
            event_from_wire(ProtocolId::Signal, relative),
            Some(AdapterEvent::VoiceFailed { file_id: 9, .. })
        ));
    }

    #[test]
    fn a_voice_stop_does_not_cross_the_pipe() {
        let event = AdapterEvent::VoiceFailed {
            protocol: ProtocolId::Signal,
            file_id: 9,
            stop: crate::media::VoiceStop::SlackFilesDenied,
        };
        let wire = event_to_wire(ProtocolId::Signal, event).expect("crosses");
        let line = thinwire_ipc::encode(&thinwire_ipc::HelperLine::Event {
            protocol: thinwire_ipc::WireProtocol::Signal,
            event: wire.clone(),
        })
        .expect("line");
        let value: serde_json::Value = serde_json::from_str(line.trim_end()).expect("json");
        let payload = value["event"].as_object().expect("event");
        assert_eq!(
            payload.get("name").and_then(|name| name.as_str()),
            Some("voice_failed")
        );
        assert_eq!(payload.get("file_id").and_then(|id| id.as_i64()), Some(9));
        assert_eq!(payload.len(), 2);
        assert!(matches!(
            event_from_wire(ProtocolId::Signal, wire),
            Some(AdapterEvent::VoiceFailed {
                file_id: 9,
                stop: crate::media::VoiceStop::Unplayable,
                ..
            })
        ));
    }

    /// A chat picture crosses as image bytes and comes back for its chat.
    /// An empty picture does not cross.
    #[test]
    fn a_chat_picture_crosses_the_pipe() {
        let event = AdapterEvent::ChatAvatar {
            protocol: ProtocolId::Signal,
            conversation_id: "signal:1".into(),
            bytes: ImageBytes::new(vec![0xFF, 0xD8, 1]),
        };
        let wire = event_to_wire(ProtocolId::Signal, event).expect("crosses");
        let Some(AdapterEvent::ChatAvatar {
            conversation_id,
            bytes,
            ..
        }) = event_from_wire(ProtocolId::Signal, wire)
        else {
            panic!("a chat picture");
        };
        assert_eq!(conversation_id, "signal:1");
        assert_eq!(bytes.as_slice(), [0xFF, 0xD8, 1]);
        let empty = AdapterEvent::ChatAvatar {
            protocol: ProtocolId::Signal,
            conversation_id: "signal:1".into(),
            bytes: ImageBytes::new(Vec::new()),
        };
        assert!(event_to_wire(ProtocolId::Signal, empty).is_none());
    }

    /// A sticker image crosses the pipe with its size and its bytes. An
    /// image that is too large for one line does not cross.
    #[test]
    fn a_message_photo_survives_the_wire() {
        let mut sticker = message(ProtocolId::WhatsApp);
        sticker.body = String::new();
        sticker.photo = photo_from_wire(WirePhoto {
            width: 512,
            height: 400,
            bytes: vec![1, 2, 3],
        });
        let event = AdapterEvent::MessageReceived {
            message: sticker.clone(),
        };
        let wire = event_to_wire(ProtocolId::WhatsApp, event.clone()).expect("wire");
        assert_eq!(event_from_wire(ProtocolId::WhatsApp, wire), Some(event));

        let mut large = sticker;
        large.photo = photo_from_wire(WirePhoto {
            width: 512,
            height: 512,
            bytes: vec![0; WirePhoto::MAX_BYTES + 1],
        });
        assert_eq!(message_to_wire(large).photo, None);
    }

    /// Every WhatsApp event that the helper can send comes back unchanged.
    #[test]
    fn whatsapp_events_survive_the_wire() {
        let protocol = ProtocolId::WhatsApp;
        let events = [
            AdapterEvent::Status {
                protocol,
                status: AdapterStatus::Ready,
                detail: "connected".into(),
            },
            AdapterEvent::Account {
                protocol,
                state: AccountState::Linked,
            },
            AdapterEvent::AccountEnded { protocol },
            AdapterEvent::ConversationUpsert {
                conversation: conversation(protocol),
            },
            AdapterEvent::ConversationRemoved {
                protocol,
                id: CHAT.into(),
            },
            AdapterEvent::MessageReceived {
                message: message(protocol),
            },
            AdapterEvent::MessageReplaced {
                protocol,
                conversation_id: CHAT.into(),
                old_id: "pending:1".into(),
                message: message(protocol),
            },
            AdapterEvent::MessageBody {
                protocol,
                conversation_id: CHAT.into(),
                message_id: "m1".into(),
                body: "edited".into(),
            },
            AdapterEvent::MessagesRemoved {
                protocol,
                conversation_id: CHAT.into(),
                message_ids: vec!["m1".into(), "m2".into()],
            },
            AdapterEvent::MessageDelivery {
                protocol,
                conversation_id: CHAT.into(),
                message_id: "m1".into(),
                delivery: Delivery::Failed,
            },
            AdapterEvent::SendAccepted {
                protocol,
                conversation_id: CHAT.into(),
                request: 4,
            },
            AdapterEvent::SendRejected {
                protocol,
                conversation_id: CHAT.into(),
                request: 5,
            },
            AdapterEvent::CommandFailed {
                protocol,
                conversation_id: Some(CHAT.into()),
                detail: "not in the list".into(),
            },
            AdapterEvent::Notice {
                protocol,
                text: "note".into(),
            },
            AdapterEvent::ChatListLoaded { protocol },
            AdapterEvent::HistoryLoaded {
                protocol,
                conversation_id: CHAT.into(),
                store_cursor: Some(HistoryStoreCursor {
                    before_ms: 10,
                    skipped: 1,
                    resume: true,
                    runs: Vec::new(),
                }),
            },
            AdapterEvent::OlderHistoryLoaded {
                protocol,
                conversation_id: CHAT.into(),
                before_message_id: "m1".into(),
                more: true,
                note: Some("try again".into()),
                topic: None,
                store_cursor: Some(HistoryStoreCursor {
                    before_ms: 9,
                    skipped: 208,
                    resume: true,
                    runs: vec![HistorySkipRun {
                        after_id: Some("8000:ada".into()),
                        skipped: 208,
                    }],
                }),
            },
            AdapterEvent::WhatsAppQr {
                code: RedactedPairingSecret::new("qr-fixture"),
                generation: 3,
            },
            AdapterEvent::WhatsAppPairCode {
                code: RedactedPairingSecret::new("ABCD-1234"),
                generation: 3,
            },
        ];
        for event in events {
            let wire = event_to_wire(protocol, event.clone()).expect("crosses the pipe");
            let line = thinwire_ipc::encode(&wire).expect("encode");
            let wire: WireEvent = thinwire_ipc::decode(line.trim_end()).expect("decode");
            assert_eq!(event_from_wire(protocol, wire), Some(event));
        }
    }

    /// A helper cannot send an event for another protocol, and events that
    /// only the app makes do not cross the pipe.
    #[test]
    fn events_of_another_protocol_and_app_events_do_not_cross() {
        let own = ProtocolId::WhatsApp;
        for event in [
            AdapterEvent::Account {
                protocol: ProtocolId::Telegram,
                state: AccountState::Linked,
            },
            AdapterEvent::MessageReceived {
                message: message(ProtocolId::Telegram),
            },
            AdapterEvent::ConversationUpsert {
                conversation: conversation(ProtocolId::Signal),
            },
            AdapterEvent::SignalQr {
                code: RedactedPairingSecret::new("sgnl://fixture"),
                generation: 1,
            },
            AdapterEvent::Stopped { protocol: own },
            AdapterEvent::Helper {
                protocol: own,
                state: crate::HelperState::Running,
            },
            AdapterEvent::FlushSecrets,
            AdapterEvent::TelegramSessionEnded,
        ] {
            assert_eq!(event_to_wire(own, event.clone()), None, "{event:?}");
        }
        // The app gives every event the protocol of its helper.
        let from_helper = event_from_wire(ProtocolId::Signal, WireEvent::ChatListLoaded);
        assert_eq!(
            from_helper,
            Some(AdapterEvent::ChatListLoaded {
                protocol: ProtocolId::Signal
            })
        );
        assert_eq!(
            event_from_wire(
                ProtocolId::Signal,
                WireEvent::PairCode {
                    code: SecretText::new("ABCD-1234"),
                    generation: 1,
                }
            ),
            None,
            "Signal has no pair code"
        );
    }

    /// Every command of the shell for WhatsApp comes back unchanged. The
    /// phone crosses only with a pairing start.
    #[test]
    fn whatsapp_commands_survive_the_wire() {
        let protocol = ProtocolId::WhatsApp;
        let commands = [
            AdapterCommand::Connect { protocol },
            AdapterCommand::Disconnect { protocol },
            AdapterCommand::LoadChats { protocol },
            AdapterCommand::OpenChat {
                protocol,
                conversation_id: CHAT.into(),
            },
            AdapterCommand::LoadOlderMessages {
                protocol,
                conversation_id: CHAT.into(),
                before_message_id: "m1".into(),
                store_cursor: None,
            },
            AdapterCommand::SendText {
                protocol,
                conversation_id: CHAT.into(),
                body: "hello".into(),
                request: 8,
                reply_to: None,
            },
            AdapterCommand::ResendMessage {
                protocol,
                conversation_id: CHAT.into(),
                message_id: "pending:1".into(),
                request: 9,
            },
            AdapterCommand::ViewChat {
                protocol,
                conversation_id: None,
            },
            AdapterCommand::WhatsAppAcknowledgeRisk,
            AdapterCommand::WhatsAppBeginLink { generation: 6 },
            AdapterCommand::WhatsAppCancelLink,
        ];
        for command in commands {
            let begins = matches!(command, AdapterCommand::WhatsAppBeginLink { .. });
            let wire = command_to_wire(protocol, &command, || Some("15550100".into()))
                .expect("crosses the pipe");
            let line = thinwire_ipc::encode(&wire).expect("encode");
            let wire: WireCommand = thinwire_ipc::decode(line.trim_end()).expect("decode");
            let (back, phone) = command_from_wire(protocol, wire).expect("a WhatsApp command");
            assert_eq!(back, command);
            assert_eq!(
                phone.as_ref().map(SecretText::expose),
                begins.then_some("15550100"),
                "{command:?}"
            );
        }
    }

    #[test]
    fn commands_that_the_app_answers_do_not_cross() {
        let protocol = ProtocolId::WhatsApp;
        for command in [
            AdapterCommand::Shutdown { protocol },
            AdapterCommand::RestartHelper { protocol },
            AdapterCommand::LoadChats {
                protocol: ProtocolId::Telegram,
            },
            AdapterCommand::SignalBeginLink { generation: 1 },
            AdapterCommand::DownloadFile {
                protocol,
                file_id: 7,
                secret: false,
            },
        ] {
            assert_eq!(command_to_wire(protocol, &command, || None), None);
        }
        assert!(wire_protocol(ProtocolId::Telegram).is_none());
        assert_eq!(
            wire_protocol(ProtocolId::WhatsApp),
            Some(WireProtocol::WhatsApp)
        );
    }

    #[test]
    fn a_history_cursor_keeps_at_most_the_scan_cap_of_runs() {
        let runs = (0..MAX_HISTORY_CURSOR_RUNS + 5)
            .map(|n| WireHistorySkip {
                after_id: Some(format!("m{n}")),
                skipped: 1,
            })
            .collect();
        let skipped = u32::try_from(MAX_HISTORY_CURSOR_RUNS + 5).expect("cap");
        let cursor = cursor_from_wire(WireHistoryCursor {
            before_ms: 1,
            skipped,
            resume: false,
            runs,
        });
        assert!(cursor.runs.is_empty(), "a cut list no longer adds up");
        assert_eq!(cursor.skipped, skipped);
        let honest: Vec<WireHistorySkip> = (0..MAX_HISTORY_CURSOR_RUNS)
            .map(|n| WireHistorySkip {
                after_id: Some(format!("m{n}")),
                skipped: 1,
            })
            .collect();
        let kept = cursor_from_wire(WireHistoryCursor {
            before_ms: 1,
            skipped: u32::try_from(MAX_HISTORY_CURSOR_RUNS).expect("cap"),
            resume: false,
            runs: honest,
        });
        assert_eq!(kept.runs.len(), MAX_HISTORY_CURSOR_RUNS);
    }

    #[test]
    fn history_runs_that_do_not_add_up_are_dropped() {
        let cursor = cursor_from_wire(WireHistoryCursor {
            before_ms: 4,
            skipped: 9,
            resume: false,
            runs: vec![
                WireHistorySkip {
                    after_id: Some("8000:ada".into()),
                    skipped: 2,
                },
                WireHistorySkip {
                    after_id: Some("6000:ada".into()),
                    skipped: 2,
                },
            ],
        });
        assert!(cursor.runs.is_empty());
        assert_eq!(cursor.skipped, 9);
    }
}
