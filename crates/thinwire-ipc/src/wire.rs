//! The lines of the wire protocol.
//!
//! `Debug` of a command, an event, a chat, or a message shows its name only.
//! Chat ids hold phone numbers, and bodies hold message text. A log line of
//! a wire value can thus never show user data.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::SecretText;

/// Version of the wire protocol. The app refuses a helper with another
/// version.
pub const PROTOCOL_VERSION: u32 = 1;

/// A protocol that a helper carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WireProtocol {
    #[serde(rename = "whatsapp")]
    WhatsApp,
    #[serde(rename = "signal")]
    Signal,
}

/// One line from the app to a helper.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AppLine {
    /// One command. The helper answers `Ack` with the same `id` when its
    /// adapter took the command. The results come as `Event` lines.
    Request {
        id: u64,
        protocol: WireProtocol,
        command: WireCommand,
    },
    /// The app closes. The helper closes its sessions, sends `Stopped`, and
    /// exits. This is not a log out: the session store stays.
    Shutdown,
}

/// One line from a helper to the app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum HelperLine {
    /// The first line of a helper.
    Hello {
        protocol_version: u32,
        helper_version: String,
        protocols: Vec<WireProtocol>,
    },
    /// The helper cannot run. It exits after this line, and it sends no
    /// `Hello`. The app does not start it again by itself.
    Refused { reason: HelperRefusal },
    /// The adapter took the request with this id.
    Ack { id: u64 },
    /// One adapter event.
    Event {
        protocol: WireProtocol,
        event: WireEvent,
    },
    /// Every session of this protocol closed after `Shutdown`.
    Stopped { protocol: WireProtocol },
}

/// Why a helper refused to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HelperRefusal {
    /// Another helper process holds the lock of the session store.
    SessionInUse,
    /// The helper found no place for its session store.
    NoDataDir,
}

/// A command for the adapter in the helper. It is a wire copy of the
/// `AdapterCommand` variants that a helper protocol uses.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "name", rename_all = "snake_case", deny_unknown_fields)]
pub enum WireCommand {
    Connect,
    /// A log out: the helper ends the session and deletes its session store.
    Disconnect,
    LoadChats,
    OpenChat {
        conversation_id: String,
    },
    LoadOlderMessages {
        conversation_id: String,
        before_message_id: String,
    },
    SendText {
        conversation_id: String,
        body: String,
        request: u64,
    },
    ResendMessage {
        conversation_id: String,
        message_id: String,
        request: u64,
    },
    /// The chat the user now looks at. `None`: the user left every chat.
    ViewChat {
        conversation_id: Option<String>,
    },
    /// The user accepted the full-screen gate of this protocol in the app.
    AcknowledgeGate,
    /// Start pairing. `phone` is the optional number for a pair code. It is
    /// the only place where a phone number crosses the pipe.
    BeginLink {
        generation: u64,
        phone: Option<SecretText>,
    },
    /// Stop a pairing that is not complete.
    CancelLink,
}

impl WireCommand {
    /// The name of the command, for logs. It holds no user data.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Connect => "connect",
            Self::Disconnect => "disconnect",
            Self::LoadChats => "load_chats",
            Self::OpenChat { .. } => "open_chat",
            Self::LoadOlderMessages { .. } => "load_older_messages",
            Self::SendText { .. } => "send_text",
            Self::ResendMessage { .. } => "resend_message",
            Self::ViewChat { .. } => "view_chat",
            Self::AcknowledgeGate => "acknowledge_gate",
            Self::BeginLink { .. } => "begin_link",
            Self::CancelLink => "cancel_link",
        }
    }
}

impl fmt::Debug for WireCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "WireCommand({})", self.kind())
    }
}

/// Live adapter state. A wire copy of `AdapterStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireStatus {
    Stubbed,
    Connecting,
    Ready,
    Refused,
    Error,
}

/// Link state of the account. A wire copy of `AccountState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireAccountState {
    Unlinked,
    Linking,
    Linked,
}

/// Delivery of an outgoing message. A wire copy of `Delivery`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireDelivery {
    Sent,
    Pending,
    Failed,
}

/// How a message reached the helper. A wire copy of `Arrival`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireArrival {
    History,
    Live,
}

/// A chat row. The protocol comes from the `Event` line.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireConversation {
    pub id: String,
    pub title: String,
    pub participant: String,
    pub preview: String,
    pub unread: u32,
    pub order: i64,
    /// Unix seconds of the last message. Zero when unknown.
    pub last_at: i64,
    pub is_group: bool,
    pub writable: bool,
    pub placeholder: bool,
    pub muted: bool,
}

impl fmt::Debug for WireConversation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WireConversation(<redacted>)")
    }
}

/// A message. The protocol comes from the `Event` line.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireMessage {
    pub conversation_id: String,
    pub id: String,
    pub sender: String,
    pub body: String,
    pub outbound: bool,
    pub delivery: WireDelivery,
    /// Unix seconds when the message was sent. Zero when unknown.
    pub sent_at: i64,
    pub arrival: WireArrival,
}

impl fmt::Debug for WireMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WireMessage(<redacted>)")
    }
}

/// An event of the adapter in the helper. It is a wire copy of the
/// `AdapterEvent` variants that a helper protocol can send.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "name", rename_all = "snake_case", deny_unknown_fields)]
pub enum WireEvent {
    Status {
        status: WireStatus,
        detail: String,
    },
    Account {
        state: WireAccountState,
    },
    /// The account ended for good: the user removed this device.
    AccountEnded,
    ConversationUpsert {
        conversation: WireConversation,
    },
    ConversationRemoved {
        id: String,
    },
    MessageReceived {
        message: WireMessage,
    },
    MessageReplaced {
        conversation_id: String,
        old_id: String,
        message: WireMessage,
    },
    MessageBody {
        conversation_id: String,
        message_id: String,
        body: String,
    },
    MessagesRemoved {
        conversation_id: String,
        message_ids: Vec<String>,
    },
    MessageDelivery {
        conversation_id: String,
        message_id: String,
        delivery: WireDelivery,
    },
    SendAccepted {
        conversation_id: String,
        request: u64,
    },
    SendRejected {
        conversation_id: String,
        request: u64,
    },
    CommandFailed {
        conversation_id: Option<String>,
        detail: String,
    },
    Notice {
        text: String,
    },
    ChatListLoaded,
    HistoryLoaded {
        conversation_id: String,
    },
    OlderHistoryLoaded {
        conversation_id: String,
        before_message_id: String,
        more: bool,
        note: Option<String>,
    },
    /// A QR payload of the pairing `generation`.
    Qr {
        code: SecretText,
        generation: u64,
    },
    /// A pair code of the pairing `generation`.
    PairCode {
        code: SecretText,
        generation: u64,
    },
}

impl WireEvent {
    /// The name of the event, for logs. It holds no user data.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Status { .. } => "status",
            Self::Account { .. } => "account",
            Self::AccountEnded => "account_ended",
            Self::ConversationUpsert { .. } => "conversation_upsert",
            Self::ConversationRemoved { .. } => "conversation_removed",
            Self::MessageReceived { .. } => "message_received",
            Self::MessageReplaced { .. } => "message_replaced",
            Self::MessageBody { .. } => "message_body",
            Self::MessagesRemoved { .. } => "messages_removed",
            Self::MessageDelivery { .. } => "message_delivery",
            Self::SendAccepted { .. } => "send_accepted",
            Self::SendRejected { .. } => "send_rejected",
            Self::CommandFailed { .. } => "command_failed",
            Self::Notice { .. } => "notice",
            Self::ChatListLoaded => "chat_list_loaded",
            Self::HistoryLoaded { .. } => "history_loaded",
            Self::OlderHistoryLoaded { .. } => "older_history_loaded",
            Self::Qr { .. } => "qr",
            Self::PairCode { .. } => "pair_code",
        }
    }
}

impl fmt::Debug for WireEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "WireEvent({})", self.kind())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{decode, encode};

    /// A phone number, a chat id with a phone number, and a message text.
    const PHONE: &str = "15550100";
    const CHAT: &str = "whatsapp:15550199@s.whatsapp.net";
    const BODY: &str = "wire-body-fixture";

    fn message() -> WireMessage {
        WireMessage {
            conversation_id: CHAT.into(),
            id: "m1".into(),
            sender: "Ana".into(),
            body: BODY.into(),
            outbound: false,
            delivery: WireDelivery::Sent,
            sent_at: 1_790_000_000,
            arrival: WireArrival::Live,
        }
    }

    #[test]
    fn a_request_has_the_documented_json_shape() {
        let line = AppLine::Request {
            id: 7,
            protocol: WireProtocol::WhatsApp,
            command: WireCommand::OpenChat {
                conversation_id: CHAT.into(),
            },
        };
        assert_eq!(
            encode(&line).expect("encode"),
            format!(
                "{{\"type\":\"request\",\"id\":7,\"protocol\":\"whatsapp\",\"command\":{{\"name\":\"open_chat\",\"conversation_id\":\"{CHAT}\"}}}}\n"
            )
        );
        assert_eq!(
            encode(&AppLine::Shutdown).expect("encode"),
            "{\"type\":\"shutdown\"}\n"
        );
    }

    #[test]
    fn every_line_survives_a_round_trip() {
        let app = [
            AppLine::Request {
                id: u64::MAX,
                protocol: WireProtocol::WhatsApp,
                command: WireCommand::BeginLink {
                    generation: 3,
                    phone: Some(SecretText::new(PHONE)),
                },
            },
            AppLine::Request {
                id: 2,
                protocol: WireProtocol::Signal,
                command: WireCommand::ViewChat {
                    conversation_id: None,
                },
            },
            AppLine::Request {
                id: 3,
                protocol: WireProtocol::WhatsApp,
                command: WireCommand::SendText {
                    conversation_id: CHAT.into(),
                    body: BODY.into(),
                    request: 900_001,
                },
            },
            AppLine::Shutdown,
        ];
        for line in app {
            let text = encode(&line).expect("encode");
            assert_eq!(decode::<AppLine>(text.trim_end()).expect("decode"), line);
        }
        let helper = [
            HelperLine::Hello {
                protocol_version: PROTOCOL_VERSION,
                helper_version: "0.1.0".into(),
                protocols: vec![WireProtocol::WhatsApp],
            },
            HelperLine::Refused {
                reason: HelperRefusal::SessionInUse,
            },
            HelperLine::Ack { id: 9 },
            HelperLine::Event {
                protocol: WireProtocol::WhatsApp,
                event: WireEvent::MessageReceived { message: message() },
            },
            HelperLine::Event {
                protocol: WireProtocol::WhatsApp,
                event: WireEvent::Qr {
                    code: SecretText::new("qr-fixture"),
                    generation: 4,
                },
            },
            HelperLine::Event {
                protocol: WireProtocol::WhatsApp,
                event: WireEvent::ChatListLoaded,
            },
            HelperLine::Stopped {
                protocol: WireProtocol::WhatsApp,
            },
        ];
        for line in helper {
            let text = encode(&line).expect("encode");
            assert_eq!(decode::<HelperLine>(text.trim_end()).expect("decode"), line);
        }
    }

    /// ADR 0012 section 4: a helper closes on a line with an unknown type,
    /// an unknown protocol, or an unknown field.
    #[test]
    fn an_unknown_type_protocol_or_field_does_not_decode() {
        for bad in [
            r#"{"type":"reboot"}"#,
            r#"{"type":"request","id":1,"protocol":"telegram","command":{"name":"connect"}}"#,
            r#"{"type":"request","id":1,"protocol":"whatsapp","command":{"name":"format_disk"}}"#,
            r#"{"type":"request","id":1,"protocol":"whatsapp","command":{"name":"connect"},"extra":1}"#,
            r#"{"type":"request","id":-1,"protocol":"whatsapp","command":{"name":"connect"}}"#,
            r#"["request"]"#,
            "not json",
            "",
        ] {
            assert!(decode::<AppLine>(bad).is_err(), "decoded: {bad}");
        }
    }

    /// Neither side logs a phone number, a chat id, or a message text
    /// (ADR 0012 "Secrets on the wire").
    #[test]
    fn debug_shows_no_user_data() {
        let lines = [
            format!(
                "{:?}",
                AppLine::Request {
                    id: 1,
                    protocol: WireProtocol::WhatsApp,
                    command: WireCommand::BeginLink {
                        generation: 1,
                        phone: Some(SecretText::new(PHONE)),
                    },
                }
            ),
            format!(
                "{:?}",
                AppLine::Request {
                    id: 2,
                    protocol: WireProtocol::WhatsApp,
                    command: WireCommand::SendText {
                        conversation_id: CHAT.into(),
                        body: BODY.into(),
                        request: 1,
                    },
                }
            ),
            format!(
                "{:?}",
                HelperLine::Event {
                    protocol: WireProtocol::WhatsApp,
                    event: WireEvent::MessageReceived { message: message() },
                }
            ),
            format!("{:?}", message()),
        ];
        for shown in lines {
            for secret in [PHONE, "15550199", BODY, "Ana"] {
                assert!(!shown.contains(secret), "{shown}");
            }
        }
    }
}
