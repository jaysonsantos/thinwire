//! Maps between the adapter contract and the wire types of `thinwire-ipc`.
//!
//! The protocol of an event comes from the helper the line came from, never
//! from the line body. A helper thus cannot send an event for another
//! protocol.

use thinwire_ipc::{
    SecretText, WireAccountState, WireArrival, WireCommand, WireConversation, WireDelivery,
    WireEvent, WireMessage, WireProtocol, WireStatus,
};

use crate::adapter::{
    AccountState, AdapterCommand, AdapterEvent, AdapterStatus, Arrival, ChatMessage, Conversation,
    Delivery, ProtocolId, RedactedPairingSecret,
};

/// The wire name of a protocol that runs in a helper. `None` for a protocol
/// that runs in the app.
#[must_use]
pub(crate) const fn wire_protocol(protocol: ProtocolId) -> Option<WireProtocol> {
    match protocol {
        ProtocolId::WhatsApp => Some(WireProtocol::WhatsApp),
        ProtocolId::Signal => Some(WireProtocol::Signal),
        ProtocolId::Telegram | ProtocolId::Discord | ProtocolId::Slack => None,
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
            ..
        } => WireCommand::LoadOlderMessages {
            conversation_id: conversation_id.clone(),
            before_message_id: before_message_id.clone(),
        },
        AdapterCommand::SendText {
            conversation_id,
            body,
            request,
            ..
        } => WireCommand::SendText {
            conversation_id: conversation_id.clone(),
            body: body.clone(),
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
        // The app answers these itself. They never cross the pipe.
        AdapterCommand::Shutdown { .. }
        | AdapterCommand::RestartHelper { .. }
        | AdapterCommand::ConnectDiscord { .. }
        | AdapterCommand::TelegramAuth { .. } => return None,
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
        } => AdapterCommand::LoadOlderMessages {
            protocol,
            conversation_id,
            before_message_id,
        },
        WireCommand::SendText {
            conversation_id,
            body,
            request,
        } => AdapterCommand::SendText {
            protocol,
            conversation_id,
            body,
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
        AdapterEvent::ChatListLoaded { protocol: seen } if own(seen) => WireEvent::ChatListLoaded,
        AdapterEvent::HistoryLoaded {
            protocol: seen,
            conversation_id,
        } if own(seen) => WireEvent::HistoryLoaded { conversation_id },
        AdapterEvent::OlderHistoryLoaded {
            protocol: seen,
            conversation_id,
            before_message_id,
            more,
            note,
        } if own(seen) => WireEvent::OlderHistoryLoaded {
            conversation_id,
            before_message_id,
            more,
            note,
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
        WireEvent::HistoryLoaded { conversation_id } => AdapterEvent::HistoryLoaded {
            protocol,
            conversation_id,
        },
        WireEvent::OlderHistoryLoaded {
            conversation_id,
            before_message_id,
            more,
            note,
        } => AdapterEvent::OlderHistoryLoaded {
            protocol,
            conversation_id,
            before_message_id,
            more,
            note,
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
    })
}

// endregion: events

// region: values

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
        Delivery::Sent => WireDelivery::Sent,
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
    }
}

fn message_to_wire(message: ChatMessage) -> WireMessage {
    WireMessage {
        conversation_id: message.conversation_id,
        id: message.id,
        sender: message.sender,
        body: message.body,
        outbound: message.outbound,
        delivery: delivery_to_wire(message.delivery),
        sent_at: message.sent_at,
        arrival: arrival_to_wire(message.arrival),
    }
}

fn message_from_wire(protocol: ProtocolId, message: WireMessage) -> ChatMessage {
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
    }
}

// endregion: values

#[cfg(test)]
mod tests {
    use super::*;

    const CHAT: &str = "whatsapp:111@s.whatsapp.net";

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
        }
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
            },
            AdapterEvent::OlderHistoryLoaded {
                protocol,
                conversation_id: CHAT.into(),
                before_message_id: "m1".into(),
                more: true,
                note: Some("try again".into()),
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
            },
            AdapterCommand::SendText {
                protocol,
                conversation_id: CHAT.into(),
                body: "hello".into(),
                request: 8,
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
        ] {
            assert_eq!(command_to_wire(protocol, &command, || None), None);
        }
        assert!(wire_protocol(ProtocolId::Telegram).is_none());
        assert_eq!(
            wire_protocol(ProtocolId::WhatsApp),
            Some(WireProtocol::WhatsApp)
        );
    }
}
