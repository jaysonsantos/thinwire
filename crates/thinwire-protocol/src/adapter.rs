//! Shared adapter trait, commands, and events.
//!
//! Implementations run on the tokio worker and must not call UI APIs.

use std::fmt;

use tokio::sync::mpsc::UnboundedSender;

/// Unbounded event sink from a worker into the UI poller.
pub type EventTx = UnboundedSender<AdapterEvent>;

/// Telegram login epoch. The host bumps it on the UI thread when it sends
/// Telegram `Disconnect` or `Shutdown`; a worker stamps its login events with
/// the value it started with. The host drops stale ones (issue #42).
pub(crate) type LoginEpoch = std::sync::Arc<std::sync::atomic::AtomicU64>;

/// v1 protocol identifiers (S2: Signal is out of v1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProtocolId {
    Telegram,
    WhatsApp,
    Discord,
    Slack,
}

impl ProtocolId {
    pub const ALL: [Self; 4] = [Self::Telegram, Self::WhatsApp, Self::Discord, Self::Slack];

    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Telegram => "Telegram",
            Self::WhatsApp => "WhatsApp",
            Self::Discord => "Discord",
            Self::Slack => "Slack",
        }
    }
}

impl fmt::Display for ProtocolId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.display_name())
    }
}

/// Honest support language shown in the shell and adapter metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupportClass {
    /// Official path; intended supported goal.
    Supported,
    /// Unofficial or unsupported; ToS or breakage risk.
    Experimental,
    /// Official bot/OAuth inbox only; no user self-bots.
    Constrained,
}

impl SupportClass {
    #[must_use]
    pub const fn badge(self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::Experimental => "experimental",
            Self::Constrained => "constrained",
        }
    }
}

/// Static capability metadata for a protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtocolCapabilities {
    pub id: ProtocolId,
    pub support: SupportClass,
    pub short_label: &'static str,
    pub detail: &'static str,
    pub official_api: bool,
    pub allows_user_account_automation: bool,
}

/// Live adapter state reported to the UI over the event channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterStatus {
    Stubbed,
    Connecting,
    Ready,
    Refused,
    Error,
}

impl AdapterStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stubbed => "stubbed",
            Self::Connecting => "connecting",
            Self::Ready => "ready",
            Self::Refused => "refused",
            Self::Error => "error",
        }
    }
}

/// Discord authentication modes. User-account / self-bot is always refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscordAuthMode {
    Bot,
    OAuth,
    UserAccount,
}

/// Telegram login step. Credential values never travel on this command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelegramAuthStep {
    ApiCredentials,
    Phone,
    Code,
    /// Ask Telegram for a new login code (TDLib `resendAuthenticationCode`).
    ResendCode,
    TwoFactor,
    Complete,
}

impl TelegramAuthStep {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ApiCredentials => "api credentials",
            Self::Phone => "phone",
            Self::Code => "code",
            Self::ResendCode => "resend code",
            Self::TwoFactor => "2fa",
            Self::Complete => "complete",
        }
    }
}

/// Why Telegram refused a login step. Holds an error name or number only,
/// never a value the user typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelegramAuthError {
    PhoneInvalid,
    CodeInvalid,
    CodeExpired,
    PasswordInvalid,
    /// Too many tries. Telegram asks the client to wait this long.
    FloodWait {
        seconds: u32,
    },
    /// TDLib refused `setTdlibParameters`, so no login step can run.
    ClientSetup {
        code: i32,
    },
    /// Any other error. Only the numeric code crosses the channel.
    Other {
        code: i32,
    },
}

/// How Telegram delivered the login code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelegramCodeVia {
    TelegramApp,
    Sms,
    /// SMS with a word or a phrase, not digits.
    SmsWord,
    Call,
    Other,
}

impl TelegramCodeVia {
    /// `false` for a word or phrase code: the code field must keep letters.
    #[must_use]
    pub const fn digits_only(self) -> bool {
        !matches!(self, Self::SmsWord)
    }
}

/// Telegram login phase reported to the UI. Never carries credential values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelegramAuthPhase {
    NeedPhone,
    NeedCode,
    NeedTwoFactor,
    Ready,
    Unavailable,
    /// Step rejected. UI stays on the current form and clears `auth_busy`.
    Failed,
}

impl TelegramAuthPhase {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NeedPhone => "need phone",
            Self::NeedCode => "need code",
            Self::NeedTwoFactor => "need 2fa",
            Self::Ready => "ready",
            Self::Unavailable => "unavailable",
            Self::Failed => "failed",
        }
    }
}

/// Commands the UI may enqueue for the tokio worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterCommand {
    Connect {
        protocol: ProtocolId,
    },
    Disconnect {
        protocol: ProtocolId,
    },
    ConnectDiscord {
        mode: DiscordAuthMode,
    },
    /// Advances the Telegram login screens. Secrets stay in the secret vault.
    /// `epoch` is the login client this step was sent for. The host fills it.
    /// A step from an older epoch is ignored: Cancel may already have cleared
    /// the secrets.
    TelegramAuth {
        step: TelegramAuthStep,
        epoch: u64,
    },
    /// Load another page of the main chat list. No secrets.
    LoadChats {
        protocol: ProtocolId,
    },
    /// Open a chat and load recent messages. `conversation_id` is not a secret.
    OpenChat {
        protocol: ProtocolId,
        conversation_id: String,
    },
    /// The app is closing. Close every client cleanly, then send `Stopped`.
    Shutdown {
        protocol: ProtocolId,
    },
    /// Send a failed outgoing message again. Ids are not secrets.
    ResendMessage {
        protocol: ProtocolId,
        conversation_id: String,
        message_id: String,
    },
    /// Send plain text. The body is the user's message, never a credential.
    /// `request` is a local id; a rejection names it in `SendRejected`.
    SendText {
        protocol: ProtocolId,
        conversation_id: String,
        body: String,
        request: u64,
    },
    /// Records that the full-screen WhatsApp ban gate was accepted.
    /// Carries no secrets and does not open a network session.
    WhatsAppAcknowledgeRisk,
    /// Asks the worker to start experimental linked-device pairing.
    /// Phone digits, if any, stay in the memory vault. This variant has no fields.
    WhatsAppBeginLink,
    /// Stops experimental pairing and clears the in-memory risk acknowledgement.
    WhatsAppCancelLink,
}

impl AdapterCommand {
    #[must_use]
    pub const fn protocol(&self) -> ProtocolId {
        match *self {
            Self::Connect { protocol }
            | Self::Disconnect { protocol }
            | Self::LoadChats { protocol }
            | Self::OpenChat { protocol, .. }
            | Self::Shutdown { protocol }
            | Self::SendText { protocol, .. }
            | Self::ResendMessage { protocol, .. } => protocol,
            Self::ConnectDiscord { .. } => ProtocolId::Discord,
            Self::TelegramAuth { .. } => ProtocolId::Telegram,
            Self::WhatsAppAcknowledgeRisk | Self::WhatsAppBeginLink | Self::WhatsAppCancelLink => {
                ProtocolId::WhatsApp
            }
        }
    }
}

/// Events a worker may push. The UI is the only consumer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterEvent {
    Status {
        protocol: ProtocolId,
        status: AdapterStatus,
        detail: String,
    },
    ConversationUpsert {
        conversation: Conversation,
    },
    MessageReceived {
        message: ChatMessage,
    },
    /// Telegram login state machine. The UI applies this on the next poll.
    TelegramAuth {
        phase: TelegramAuthPhase,
    },
    /// Telegram refused the last login step. Sent just before the `Failed` phase.
    TelegramAuthRejected {
        error: TelegramAuthError,
    },
    /// The old Telegram data folder could not open (its key was lost). It was
    /// moved aside, and a fresh login follows. Carries no path or value.
    TelegramDataReset {
        /// Name of the moved-aside folder, for example `tdlib.stale-1790000000`.
        /// A file name only, never a path.
        moved_to: String,
    },
    /// A live session ended without a request from this app (remote logout,
    /// or the session was revoked). The client closes; a new login follows.
    TelegramSessionEnded,
    /// A login event stamped with its client's login epoch. Internal: the host
    /// unwraps it in `poll_events` and drops it when the epoch is stale, so the
    /// UI never sees this variant.
    Login {
        epoch: u64,
        event: Box<AdapterEvent>,
    },
    /// Telegram sent a login code. Sent just before the `NeedCode` phase.
    TelegramCodeSent {
        via: TelegramCodeVia,
    },
    /// Ask the UI to flush persistent vault keys to the OS keychain.
    /// Never carries secret values.
    FlushSecrets,
    /// Drop a chat that left the main list (`order == 0`).
    ConversationRemoved {
        protocol: ProtocolId,
        id: String,
    },
    /// A pending outgoing id was replaced by the sent message id.
    MessageReplaced {
        protocol: ProtocolId,
        conversation_id: String,
        old_id: String,
        message: ChatMessage,
    },
    /// Edit the body of a message already in the thread. Does not change sender.
    MessageBody {
        protocol: ProtocolId,
        conversation_id: String,
        message_id: String,
        body: String,
    },
    /// Drop messages TDLib deleted. They stay gone for this session.
    MessagesRemoved {
        protocol: ProtocolId,
        conversation_id: String,
        message_ids: Vec<String>,
    },
    /// The adapter accepted this send: its pending message exists. Only this
    /// event clears the draft; a history message with the same text does not.
    SendAccepted {
        protocol: ProtocolId,
        conversation_id: String,
        request: u64,
    },
    /// The adapter did not accept this send (no pending message exists). Only
    /// this event fails the send; other errors leave it pending.
    SendRejected {
        protocol: ProtocolId,
        conversation_id: String,
        request: u64,
    },
    /// Every client of this protocol closed after `Shutdown`. The app may exit.
    Stopped {
        protocol: ProtocolId,
    },
    /// New delivery state for a message already in the thread.
    MessageDelivery {
        protocol: ProtocolId,
        conversation_id: String,
        message_id: String,
        delivery: Delivery,
    },
    /// A chat-list page load ended (loaded, already complete, or failed).
    /// The UI stops its "Loading chats…" state.
    ChatListLoaded {
        protocol: ProtocolId,
    },
    /// A history load for one chat ended. The UI stops "Loading messages…".
    HistoryLoaded {
        protocol: ProtocolId,
        conversation_id: String,
    },
    /// Experimental WhatsApp QR payload. Debug output is redacted.
    /// Never log [`RedactedPairingSecret::reveal`].
    WhatsAppQr {
        code: RedactedPairingSecret,
        /// Link generation that produced this payload. Stale generations are dropped.
        generation: u64,
    },
    /// Experimental WhatsApp pair code. Debug output is redacted.
    /// Never log [`RedactedPairingSecret::reveal`].
    WhatsAppPairCode {
        code: RedactedPairingSecret,
        /// Link generation that produced this payload. Stale generations are dropped.
        generation: u64,
    },
}

impl AdapterEvent {
    /// The protocol of an inbox event: chats, messages, sends, and list
    /// loads. `None` for login, status, and session events. A frontend drops
    /// an inbox event of an account that is not linked (PR #49 review).
    #[must_use]
    pub fn inbox_protocol(&self) -> Option<ProtocolId> {
        match self {
            Self::ConversationUpsert { conversation } => Some(conversation.protocol),
            Self::MessageReceived { message } => Some(message.protocol),
            Self::ConversationRemoved { protocol, .. }
            | Self::MessageReplaced { protocol, .. }
            | Self::MessageBody { protocol, .. }
            | Self::MessagesRemoved { protocol, .. }
            | Self::SendAccepted { protocol, .. }
            | Self::SendRejected { protocol, .. }
            | Self::MessageDelivery { protocol, .. }
            | Self::ChatListLoaded { protocol }
            | Self::HistoryLoaded { protocol, .. } => Some(*protocol),
            _ => None,
        }
    }
}

/// Pairing material shown only on the experimental WhatsApp screen.
///
/// `Debug` is redacted. Do not put this value on [`AdapterCommand`].
#[derive(Clone, PartialEq, Eq)]
pub struct RedactedPairingSecret {
    value: String,
}

impl RedactedPairingSecret {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
        }
    }

    /// UI-only access. Callers must not log or persist the returned string.
    #[must_use]
    pub fn reveal(&self) -> &str {
        &self.value
    }
}

impl fmt::Debug for RedactedPairingSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RedactedPairingSecret(<redacted>)")
    }
}

/// Conversation row shown in the inbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conversation {
    pub protocol: ProtocolId,
    pub id: String,
    pub title: String,
    pub participant: String,
    pub preview: String,
    pub unread: u32,
    /// TDLib main-list order. Higher sorts first. Zero means unordered.
    pub order: i64,
    /// Unix seconds of the last message. Zero when unknown.
    pub last_at: i64,
    /// Group chat: the thread names each run of senders.
    pub is_group: bool,
}

/// Delivery of an outgoing message. Incoming messages are always `Sent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Delivery {
    #[default]
    Sent,
    /// Queued on the client. The server has not confirmed it yet.
    Pending,
    /// The server did not accept it. The user can retry.
    Failed,
}

/// Message shown in the right pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessage {
    pub protocol: ProtocolId,
    pub conversation_id: String,
    pub id: String,
    pub sender: String,
    pub body: String,
    pub outbound: bool,
    pub delivery: Delivery,
    /// Unix seconds when the message was sent. Zero when unknown.
    pub sent_at: i64,
}

/// Recoverable adapter failure. Never includes secrets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterError {
    Refused {
        protocol: ProtocolId,
        reason: &'static str,
    },
    Unavailable {
        protocol: ProtocolId,
        reason: &'static str,
    },
}

impl fmt::Display for AdapterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused { protocol, reason } => {
                write!(f, "{protocol} refused: {reason}")
            }
            Self::Unavailable { protocol, reason } => {
                write!(f, "{protocol} unavailable: {reason}")
            }
        }
    }
}

impl std::error::Error for AdapterError {}

/// Protocol I/O contract. Implementations must not call egui/eframe APIs.
pub trait ProtocolAdapter: Send {
    fn id(&self) -> ProtocolId;
    fn capabilities(&self) -> ProtocolCapabilities;
    fn start(&mut self, events: EventTx);
    fn handle(&mut self, command: AdapterCommand, events: &EventTx) -> Result<(), AdapterError>;

    /// The app is closing. Close every live session, then emit `Stopped`
    /// once. The default is for an adapter with nothing running.
    fn shutdown(&mut self, events: &EventTx) {
        emit_stopped(events, self.id());
    }
}

pub(crate) fn emit_status(
    events: &EventTx,
    protocol: ProtocolId,
    status: AdapterStatus,
    detail: impl Into<String>,
) {
    let _ = events.send(AdapterEvent::Status {
        protocol,
        status,
        detail: detail.into(),
    });
}

pub(crate) fn emit_conversation(events: &EventTx, conversation: Conversation) {
    let _ = events.send(AdapterEvent::ConversationUpsert { conversation });
}

pub(crate) fn emit_message(events: &EventTx, message: ChatMessage) {
    let _ = events.send(AdapterEvent::MessageReceived { message });
}
#[cfg_attr(not(feature = "telegram-tdlib"), allow(dead_code))]
pub(crate) fn emit_telegram_session_ended(events: &EventTx) {
    let _ = events.send(AdapterEvent::TelegramSessionEnded);
}

#[cfg_attr(not(feature = "telegram-tdlib"), allow(dead_code))]
pub(crate) fn emit_telegram_data_reset(events: &EventTx, moved_to: &str) {
    // A file name only: drop anything up to the last path separator.
    let name = moved_to.rsplit(['/', '\\']).next().unwrap_or_default();
    let _ = events.send(AdapterEvent::TelegramDataReset {
        moved_to: name.to_string(),
    });
}

#[cfg_attr(not(feature = "telegram-tdlib"), allow(dead_code))]
pub(crate) fn emit_conversation_removed(
    events: &EventTx,
    protocol: ProtocolId,
    id: impl Into<String>,
) {
    let _ = events.send(AdapterEvent::ConversationRemoved {
        protocol,
        id: id.into(),
    });
}

#[cfg_attr(not(feature = "telegram-tdlib"), allow(dead_code))]
pub(crate) fn emit_message_replaced(
    events: &EventTx,
    old_id: impl Into<String>,
    message: ChatMessage,
) {
    let _ = events.send(AdapterEvent::MessageReplaced {
        protocol: message.protocol,
        conversation_id: message.conversation_id.clone(),
        old_id: old_id.into(),
        message,
    });
}

#[cfg_attr(not(feature = "telegram-tdlib"), allow(dead_code))]
pub(crate) fn emit_messages_removed(
    events: &EventTx,
    protocol: ProtocolId,
    conversation_id: impl Into<String>,
    message_ids: Vec<String>,
) {
    let _ = events.send(AdapterEvent::MessagesRemoved {
        protocol,
        conversation_id: conversation_id.into(),
        message_ids,
    });
}

#[cfg_attr(not(feature = "telegram-tdlib"), allow(dead_code))]
pub(crate) fn emit_message_body(
    events: &EventTx,
    protocol: ProtocolId,
    conversation_id: impl Into<String>,
    message_id: impl Into<String>,
    body: impl Into<String>,
) {
    let _ = events.send(AdapterEvent::MessageBody {
        protocol,
        conversation_id: conversation_id.into(),
        message_id: message_id.into(),
        body: body.into(),
    });
}

#[cfg_attr(not(feature = "telegram-tdlib"), allow(dead_code))]
pub(crate) fn emit_message_delivery(
    events: &EventTx,
    protocol: ProtocolId,
    conversation_id: impl Into<String>,
    message_id: impl Into<String>,
    delivery: Delivery,
) {
    let _ = events.send(AdapterEvent::MessageDelivery {
        protocol,
        conversation_id: conversation_id.into(),
        message_id: message_id.into(),
        delivery,
    });
}

#[cfg_attr(not(feature = "telegram-tdlib"), allow(dead_code))]
pub(crate) fn emit_send_accepted(
    events: &EventTx,
    protocol: ProtocolId,
    conversation_id: impl Into<String>,
    request: u64,
) {
    let _ = events.send(AdapterEvent::SendAccepted {
        protocol,
        conversation_id: conversation_id.into(),
        request,
    });
}

pub(crate) fn emit_send_rejected(
    events: &EventTx,
    protocol: ProtocolId,
    conversation_id: impl Into<String>,
    request: u64,
) {
    let _ = events.send(AdapterEvent::SendRejected {
        protocol,
        conversation_id: conversation_id.into(),
        request,
    });
}

pub(crate) fn emit_stopped(events: &EventTx, protocol: ProtocolId) {
    let _ = events.send(AdapterEvent::Stopped { protocol });
}

#[cfg_attr(not(feature = "telegram-tdlib"), allow(dead_code))]
pub(crate) fn emit_chat_list_loaded(events: &EventTx, protocol: ProtocolId) {
    let _ = events.send(AdapterEvent::ChatListLoaded { protocol });
}

#[cfg_attr(not(feature = "telegram-tdlib"), allow(dead_code))]
pub(crate) fn emit_history_loaded(
    events: &EventTx,
    protocol: ProtocolId,
    conversation_id: impl Into<String>,
) {
    let _ = events.send(AdapterEvent::HistoryLoaded {
        protocol,
        conversation_id: conversation_id.into(),
    });
}

#[cfg_attr(not(feature = "telegram-tdlib"), allow(dead_code))]
pub(crate) fn emit_flush_secrets(events: &EventTx) {
    let _ = events.send(AdapterEvent::FlushSecrets);
}

#[cfg(test)]
mod inbox_protocol_tests {
    use super::*;

    #[test]
    fn inbox_events_name_their_protocol_and_login_events_do_not() {
        assert_eq!(
            AdapterEvent::ChatListLoaded {
                protocol: ProtocolId::Telegram
            }
            .inbox_protocol(),
            Some(ProtocolId::Telegram)
        );
        assert_eq!(
            AdapterEvent::ConversationRemoved {
                protocol: ProtocolId::Slack,
                id: "slack:1".into(),
            }
            .inbox_protocol(),
            Some(ProtocolId::Slack)
        );
        assert_eq!(
            AdapterEvent::TelegramAuth {
                phase: TelegramAuthPhase::Ready
            }
            .inbox_protocol(),
            None
        );
        assert_eq!(AdapterEvent::TelegramSessionEnded.inbox_protocol(), None);
        assert_eq!(
            AdapterEvent::Stopped {
                protocol: ProtocolId::Telegram
            }
            .inbox_protocol(),
            None
        );
    }
}
