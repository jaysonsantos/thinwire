//! Shared adapter trait, commands, and events.
//!
//! Implementations run on the tokio worker and must not call UI APIs.

use std::fmt;

use tokio::sync::mpsc::UnboundedSender;

/// Unbounded event sink from a worker into the UI poller.
pub type EventTx = UnboundedSender<AdapterEvent>;

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
            Self::TwoFactor => "2fa",
            Self::Complete => "complete",
        }
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
    TelegramAuth {
        step: TelegramAuthStep,
    },
}

impl AdapterCommand {
    #[must_use]
    pub const fn protocol(&self) -> ProtocolId {
        match *self {
            Self::Connect { protocol } | Self::Disconnect { protocol } => protocol,
            Self::ConnectDiscord { .. } => ProtocolId::Discord,
            Self::TelegramAuth { .. } => ProtocolId::Telegram,
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
    /// Ask the UI to flush persistent vault keys to the OS keychain.
    /// Never carries secret values.
    FlushSecrets,
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

pub(crate) fn emit_telegram_auth(events: &EventTx, phase: TelegramAuthPhase) {
    let _ = events.send(AdapterEvent::TelegramAuth { phase });
}

#[cfg_attr(not(feature = "telegram-tdlib"), allow(dead_code))]
pub(crate) fn emit_flush_secrets(events: &EventTx) {
    let _ = events.send(AdapterEvent::FlushSecrets);
}
