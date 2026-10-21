//! Shared adapter trait, commands, and events.
//!
//! Implementations run on the tokio worker and must not call UI APIs.

use std::fmt;

use tokio::sync::mpsc::UnboundedSender;

/// Unbounded event sink from a worker into the UI poller.
pub type EventTx = UnboundedSender<AdapterEvent>;

/// How long the app waits at close for every adapter to emit `Stopped`.
/// An adapter's own shutdown bound stays below it, with slack, so its
/// `Stopped` comes in time.
pub const APP_CLOSE_LIMIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Telegram login epoch. The host bumps it on the UI thread when it sends
/// Telegram `Disconnect` or `Shutdown`; a worker stamps its login events with
/// the value it started with. The host drops stale ones (issue #42).
pub(crate) type LoginEpoch = std::sync::Arc<std::sync::atomic::AtomicU64>;

/// Protocol identifiers. Signal runs in its helper process and needs feature `signal-local`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProtocolId {
    Telegram,
    WhatsApp,
    Discord,
    Slack,
    Signal,
}

impl ProtocolId {
    pub const ALL: [Self; 5] = [
        Self::Telegram,
        Self::WhatsApp,
        Self::Discord,
        Self::Slack,
        Self::Signal,
    ];

    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Telegram => "Telegram",
            Self::WhatsApp => "WhatsApp",
            Self::Discord => "Discord",
            Self::Slack => "Slack",
            Self::Signal => "Signal",
        }
    }

    /// The inbox downloads a chat photo for this protocol.
    /// The other protocols draw initials.
    #[must_use]
    pub const fn downloads_avatar(self) -> bool {
        matches!(self, Self::Telegram)
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
    /// The shell may offer Send for this protocol. A chat can still be
    /// read-only (`Conversation::writable`).
    pub sends_text: bool,
    /// The adapter answers `LoadOlderMessages`. The shell asks for older
    /// messages on scroll up only for such a protocol.
    pub pages_history: bool,
}

/// Link state of the account of one protocol.
/// [`AdapterEvent::Account`] changes it. A wrong key also leaves `Linked`.
/// A `Status` never does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AccountState {
    /// No account, or the account ended. The shell shows no rows for it.
    #[default]
    Unlinked,
    /// A login or pairing runs. Not usable yet.
    Linking,
    /// Signed in. The shell shows its chats.
    Linked,
}

/// State of the helper process of a protocol that runs out of process
/// (ADR 0013). Only [`AdapterEvent::Helper`] reports it. A protocol that
/// runs in the app never sends it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelperState {
    /// No helper runs, and none is necessary: the user has no link to
    /// keep. The account row shows its normal state.
    Idle,
    /// The helper starts, or it does not answer in time. The account row
    /// shows a busy state. The window never waits.
    Busy,
    /// The helper runs and answers.
    Running,
    /// The helper stopped. The adapter starts it again after a wait.
    Restarting,
    /// The helper stopped, and the adapter does not start it again by
    /// itself. The account row shows "Helper stopped." and Restart.
    Stopped(HelperFault),
    /// The helper program is not installed. The protocol stays visible.
    Missing,
}

/// Why a helper stopped for good.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelperFault {
    /// It stopped again and again. The restart backoff ended.
    Crashed,
    /// The system did not start the program.
    StartFailed,
    /// Its wire protocol version is not the version of this app.
    VersionMismatch,
    /// Another helper process uses the session store.
    SessionInUse,
    /// The last helper process got its kill but did not end, so a new one
    /// cannot start: two processes on one session store break it.
    StillRunning,
    /// The helper program has no client for its protocol: it was built
    /// without it.
    NoClient,
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
    /// The state in plain words, for the user. `as_str` is for logs.
    #[must_use]
    pub const fn plain_words(self) -> &'static str {
        match self {
            Self::Stubbed => "Not set up",
            Self::Connecting => "Connecting",
            Self::Ready => "Signed in",
            Self::Refused => "Sign-in refused",
            Self::Error => "Connection problem",
        }
    }

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
    /// Open one forum topic or bot thread and load its recent messages.
    OpenTopic {
        protocol: ProtocolId,
        conversation_id: String,
        topic: MessageTopic,
    },
    /// The app is closing. Close every client cleanly, then send `Stopped`.
    Shutdown {
        protocol: ProtocolId,
    },
    /// Load one page of messages older than `before_message_id` (the oldest
    /// message the UI has for this chat). Ids are not secrets.
    LoadOlderMessages {
        protocol: ProtocolId,
        conversation_id: String,
        before_message_id: String,
    },
    /// One older page of one topic. Ids are not secrets.
    LoadOlderTopic {
        protocol: ProtocolId,
        conversation_id: String,
        topic: MessageTopic,
        before_message_id: String,
    },
    /// Send a failed outgoing message again. Ids are not secrets. `request`
    /// is a local id: the adapter answers `SendAccepted` or `SendRejected`
    /// with it, as for `SendText`.
    ResendMessage {
        protocol: ProtocolId,
        conversation_id: String,
        message_id: String,
        request: u64,
    },
    /// The chat the user now looks at in this protocol. `None`: the user left
    /// every chat of this protocol. Not a load request (`OpenChat` is). The
    /// host routes it to [`ProtocolAdapter::view_chat`].
    ViewChat {
        protocol: ProtocolId,
        conversation_id: Option<String>,
    },
    /// The topic the user now looks at. The host routes it to
    /// [`ProtocolAdapter::view_topic`].
    ViewTopic {
        protocol: ProtocolId,
        conversation_id: String,
        topic: MessageTopic,
    },
    /// Send plain text. The body is the user's message, never a credential.
    /// `request` is a local id; a rejection names it in `SendRejected`.
    SendText {
        protocol: ProtocolId,
        conversation_id: String,
        body: String,
        request: u64,
    },
    /// Send plain text in one topic. The body is the user's message.
    SendTopicText {
        protocol: ProtocolId,
        conversation_id: String,
        topic: MessageTopic,
        body: String,
        request: u64,
    },
    /// Records that the full-screen WhatsApp ban gate was accepted.
    /// Carries no secrets and does not open a network session.
    WhatsAppAcknowledgeRisk,
    /// Asks the worker to start experimental linked-device pairing.
    /// Phone digits, if any, stay in the memory vault. `generation` is the
    /// shell's id for this pairing: every QR and pair-code event of it
    /// carries the same number. It is not a secret.
    WhatsAppBeginLink {
        generation: u64,
    },
    /// Stops experimental pairing and clears the in-memory risk acknowledgement.
    WhatsAppCancelLink,
    /// Records that the full-screen Signal notice was accepted.
    /// Carries no secrets and does not open a network session.
    SignalAcknowledgeNotice,
    /// Asks the worker to start secondary-device linking.
    /// The provisioning URL is not a field. It leaves later as a redacted event.
    /// `generation` is the pairing the core assigned. The QR carries the same value.
    SignalBeginLink {
        generation: u64,
    },
    /// Stops the linking and clears the in-memory notice acknowledgement.
    SignalCancelLink,
    /// Start the helper process of this protocol again after it stopped
    /// (ADR 0013). A protocol that runs in the app refuses it.
    RestartHelper {
        protocol: ProtocolId,
    },
    /// Read the session key again. This does not start a link and it does
    /// not move the store.
    RetryStoreKey {
        protocol: ProtocolId,
    },
    /// Download one Telegram photo file. `secret` means the file is deleted
    /// after the bytes are read, and the UI does not open the full view.
    /// The file id is not a secret.
    DownloadFile {
        protocol: ProtocolId,
        file_id: i32,
        secret: bool,
    },
}

impl AdapterCommand {
    #[must_use]
    pub const fn protocol(&self) -> ProtocolId {
        match *self {
            Self::Connect { protocol }
            | Self::Disconnect { protocol }
            | Self::LoadChats { protocol }
            | Self::OpenChat { protocol, .. }
            | Self::OpenTopic { protocol, .. }
            | Self::Shutdown { protocol }
            | Self::LoadOlderMessages { protocol, .. }
            | Self::LoadOlderTopic { protocol, .. }
            | Self::SendText { protocol, .. }
            | Self::SendTopicText { protocol, .. }
            | Self::ResendMessage { protocol, .. }
            | Self::ViewChat { protocol, .. }
            | Self::ViewTopic { protocol, .. }
            | Self::RestartHelper { protocol }
            | Self::RetryStoreKey { protocol }
            | Self::DownloadFile { protocol, .. } => protocol,
            Self::ConnectDiscord { .. } => ProtocolId::Discord,
            Self::TelegramAuth { .. } => ProtocolId::Telegram,
            Self::WhatsAppAcknowledgeRisk
            | Self::WhatsAppBeginLink { .. }
            | Self::WhatsAppCancelLink => ProtocolId::WhatsApp,
            Self::SignalAcknowledgeNotice
            | Self::SignalBeginLink { .. }
            | Self::SignalCancelLink => ProtocolId::Signal,
        }
    }
}

/// Why the helper could not use the session key. The account stays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreAccessReason {
    /// The keychain refused the read. The store was not moved.
    KeychainDenied,
    /// The encrypted store did not open. The folder was moved aside.
    WrongKey,
}

/// `true` when this command makes a helper read the session key.
///
/// A start with no Signal or WhatsApp account does not send one of these.
#[must_use]
pub const fn command_reads_session_key(command: &AdapterCommand) -> bool {
    matches!(
        command,
        AdapterCommand::SignalBeginLink { .. }
            | AdapterCommand::WhatsAppBeginLink { .. }
            | AdapterCommand::RetryStoreKey { .. }
    )
}

/// Events a worker may push. The UI is the only consumer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterEvent {
    Status {
        protocol: ProtocolId,
        status: AdapterStatus,
        detail: String,
    },
    /// The session key could not be used. This does not end the account.
    StoreAccess {
        protocol: ProtocolId,
        reason: StoreAccessReason,
    },
    ConversationUpsert {
        conversation: Conversation,
    },
    /// One forum topic or bot thread. It does not change which chat is open.
    TopicUpsert {
        topic: ChatTopic,
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
    /// The account of `protocol` ended for good: the user removed this
    /// device from the account, for example a WhatsApp logout on the phone.
    /// The shell drops the data it keeps for that account, such as the
    /// thinwire mutes (#153). A reconnect, a ban, a failed pairing, or a stop
    /// never sends it.
    AccountEnded {
        protocol: ProtocolId,
    },
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
    /// Replace the caption, the links, and the photo of a message. A text
    /// edit sets `photo` to `None`.
    MessageMedia {
        protocol: ProtocolId,
        conversation_id: String,
        message_id: String,
        body: String,
        links: Vec<crate::media::MessageLink>,
        photo: Option<crate::media::MessagePhoto>,
    },
    /// Links found after the message was shown. Does not change the body.
    MessageLinks {
        protocol: ProtocolId,
        conversation_id: String,
        message_id: String,
        links: Vec<crate::media::MessageLink>,
    },
    /// A photo file finished downloading.
    /// [`crate::media::PhotoFile`] has a path or bytes. An empty file is
    /// [`Self::PhotoFailed`].
    PhotoReady {
        protocol: ProtocolId,
        file_id: i32,
        file: crate::media::PhotoFile,
    },
    /// A photo file did not download.
    PhotoFailed {
        protocol: ProtocolId,
        file_id: i32,
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
    /// New link state of this protocol's account. The only event that
    /// changes it: a `Status` (also `Error`) never unlinks.
    Account {
        protocol: ProtocolId,
        state: AccountState,
    },
    /// One command failed; the session is still up. The shell stops that
    /// command's spinner and shows the error. It never ends a send (only
    /// `SendRejected` does) and never changes the link state.
    CommandFailed {
        protocol: ProtocolId,
        /// The chat of the command, if it had one.
        conversation_id: Option<String>,
        detail: String,
    },
    /// Information for the user, for example "the bot cannot read this
    /// channel". Shown as a note. Never a refusal, never an error.
    Notice {
        protocol: ProtocolId,
        text: String,
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
    /// A request for older messages ended. The older messages came before this
    /// event as `MessageReceived`. `more == false` means the start of the chat
    /// (or no more can load now): the UI stops asking.
    OlderHistoryLoaded {
        protocol: ProtocolId,
        conversation_id: String,
        before_message_id: String,
        more: bool,
        /// Why this page did not load, for this chat only. Not an account
        /// error. `None` on success, so a later success clears it (#57).
        note: Option<String>,
        /// The topic this page belongs to. `None` is a chat that has no topic.
        /// Paging state is per topic, so an answer for one topic does not end
        /// another topic of the same chat.
        topic: Option<MessageTopic>,
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
    /// Signal provisioning URL. Debug output is redacted.
    /// Never log [`RedactedPairingSecret::reveal`].
    SignalQr {
        code: RedactedPairingSecret,
        /// Link generation that produced this payload. Stale generations are dropped.
        generation: u64,
    },
    /// New state of this protocol's helper process (ADR 0013). It never
    /// changes the link state: an `Account` event does that.
    Helper {
        protocol: ProtocolId,
        state: HelperState,
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
            Self::TopicUpsert { topic } => Some(topic.protocol),
            Self::MessageReceived { message } => Some(message.protocol),
            Self::ConversationRemoved { protocol, .. }
            | Self::MessageReplaced { protocol, .. }
            | Self::MessageBody { protocol, .. }
            | Self::MessageMedia { protocol, .. }
            | Self::MessageLinks { protocol, .. }
            | Self::PhotoReady { protocol, .. }
            | Self::PhotoFailed { protocol, .. }
            | Self::MessagesRemoved { protocol, .. }
            | Self::SendAccepted { protocol, .. }
            | Self::SendRejected { protocol, .. }
            | Self::MessageDelivery { protocol, .. }
            | Self::ChatListLoaded { protocol }
            | Self::HistoryLoaded { protocol, .. }
            | Self::OlderHistoryLoaded { protocol, .. } => Some(*protocol),
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
    /// The account may post in this chat. A read-only channel is `false`.
    pub writable: bool,
    /// A stand-in row, not a real chat (for example a bot inbox with no
    /// gateway yet). The shell never auto-selects it and never sends
    /// `OpenChat` for it.
    pub placeholder: bool,
    /// The user muted this chat in the protocol (for example TDLib
    /// notification settings). A muted chat never notifies (#32).
    pub muted: bool,
    /// A forum supergroup, or a chat with a bot that has topics. The sidebar
    /// keeps one row. The center pane lists the topics.
    pub has_topics: bool,
    /// Inbox avatar. Telegram sets the small photo and the accent.
    /// Other protocols leave the photo empty.
    pub avatar: crate::media::ChatAvatar,
}

/// One topic inside a forum or a bot chat.
///
/// `Forum` is a forum topic or a bot topic (`messageTopicForum`).
/// `Thread` is a topic in a non-forum supergroup (`messageTopicThread`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MessageTopic {
    Forum(i64),
    Thread(i64),
}

/// The mute id of one topic. The mute file stays one id per line.
#[must_use]
pub fn topic_mute_id(conversation_id: &str, topic: MessageTopic) -> String {
    let mark = match topic {
        MessageTopic::Forum(id) => format!("#forum:{id}"),
        MessageTopic::Thread(id) => format!("#thread:{id}"),
    };
    format!("{conversation_id}{mark}")
}

/// One topic row of a forum or a bot chat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatTopic {
    pub protocol: ProtocolId,
    pub conversation_id: String,
    pub topic: MessageTopic,
    pub name: String,
    pub unread: u32,
    /// Unix seconds of the last message. Zero when unknown.
    pub last_at: i64,
    /// The General topic. It stays first when it is not hidden.
    pub general: bool,
    /// Hidden above the topic list. A hidden General topic is not shown.
    pub hidden: bool,
    /// A closed topic does not take new messages.
    pub closed: bool,
    /// The user muted this topic in the protocol.
    pub muted: bool,
}

/// Topics the center pane shows. Hidden topics are left out. General is
/// first. The rest follow the last message, newest first. Equal times keep
/// a stable order.
#[must_use]
pub fn ordered_topics(topics: &[ChatTopic]) -> Vec<&ChatTopic> {
    let mut rows: Vec<&ChatTopic> = topics.iter().filter(|topic| !topic.hidden).collect();
    rows.sort_by(|left, right| {
        right
            .general
            .cmp(&left.general)
            .then(right.last_at.cmp(&left.last_at))
            .then(left.topic.cmp(&right.topic))
    });
    rows
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
    /// A new message that just arrived, or a row of a history load. Only a
    /// `Live` message can notify (#32).
    pub arrival: Arrival,
    /// The forum topic or bot thread this message belongs to.
    pub topic: Option<MessageTopic>,
    /// Links inside [`Self::body`]. Empty when the text has none.
    pub links: Vec<crate::media::MessageLink>,
    /// A photo. [`Self::body`] is the caption, and it can be empty.
    pub photo: Option<crate::media::MessagePhoto>,
}

/// How a message reached the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Arrival {
    /// A history page, a chat-list load, or a refresh of a known message.
    #[default]
    History,
    /// A new message that the protocol pushed just now.
    Live,
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

    /// The user now looks at this chat (`None`: left every chat of this
    /// protocol). Default: ignore. An adapter that tracks read state or
    /// unread counts overrides it.
    fn view_chat(&mut self, conversation_id: Option<&str>, events: &EventTx) {
        let _ = (conversation_id, events);
    }

    /// The user now looks at this topic. Default: the same as looking at
    /// the chat. An adapter that tracks a topic overrides it.
    fn view_topic(&mut self, conversation_id: &str, topic: MessageTopic, events: &EventTx) {
        let _ = topic;
        self.view_chat(Some(conversation_id), events);
    }
}

pub fn emit_status(
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

pub fn emit_conversation(events: &EventTx, conversation: Conversation) {
    let _ = events.send(AdapterEvent::ConversationUpsert { conversation });
}

#[cfg_attr(not(feature = "telegram-tdlib"), allow(dead_code))]
pub(crate) fn emit_topic(events: &EventTx, topic: ChatTopic) {
    let _ = events.send(AdapterEvent::TopicUpsert { topic });
}

pub fn emit_message(events: &EventTx, message: ChatMessage) {
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
pub(crate) fn emit_message_media(
    events: &EventTx,
    protocol: ProtocolId,
    conversation_id: impl Into<String>,
    message_id: impl Into<String>,
    body: impl Into<String>,
    links: Vec<crate::media::MessageLink>,
    photo: Option<crate::media::MessagePhoto>,
) {
    let _ = events.send(AdapterEvent::MessageMedia {
        protocol,
        conversation_id: conversation_id.into(),
        message_id: message_id.into(),
        body: body.into(),
        links,
        photo,
    });
}

#[cfg_attr(not(feature = "telegram-tdlib"), allow(dead_code))]
pub(crate) fn emit_message_links(
    events: &EventTx,
    protocol: ProtocolId,
    conversation_id: impl Into<String>,
    message_id: impl Into<String>,
    links: Vec<crate::media::MessageLink>,
) {
    let _ = events.send(AdapterEvent::MessageLinks {
        protocol,
        conversation_id: conversation_id.into(),
        message_id: message_id.into(),
        links,
    });
}

#[cfg_attr(not(feature = "telegram-tdlib"), allow(dead_code))]
pub(crate) fn emit_photo_ready(
    events: &EventTx,
    protocol: ProtocolId,
    file_id: i32,
    file: crate::media::PhotoFile,
) {
    let _ = events.send(AdapterEvent::PhotoReady {
        protocol,
        file_id,
        file,
    });
}

#[cfg_attr(not(feature = "telegram-tdlib"), allow(dead_code))]
pub(crate) fn emit_photo_failed(events: &EventTx, protocol: ProtocolId, file_id: i32) {
    let _ = events.send(AdapterEvent::PhotoFailed { protocol, file_id });
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
pub fn emit_send_accepted(
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

pub fn emit_send_rejected(
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

pub fn emit_account(events: &EventTx, protocol: ProtocolId, state: AccountState) {
    let _ = events.send(AdapterEvent::Account { protocol, state });
}

pub(crate) fn emit_command_failed(
    events: &EventTx,
    protocol: ProtocolId,
    conversation_id: Option<String>,
    detail: impl Into<String>,
) {
    let _ = events.send(AdapterEvent::CommandFailed {
        protocol,
        conversation_id,
        detail: detail.into(),
    });
}

pub(crate) fn emit_notice(events: &EventTx, protocol: ProtocolId, text: impl Into<String>) {
    let _ = events.send(AdapterEvent::Notice {
        protocol,
        text: text.into(),
    });
}

pub(crate) fn emit_helper(events: &EventTx, protocol: ProtocolId, state: HelperState) {
    let _ = events.send(AdapterEvent::Helper { protocol, state });
}

pub fn emit_stopped(events: &EventTx, protocol: ProtocolId) {
    let _ = events.send(AdapterEvent::Stopped { protocol });
}

/// End one `LoadOlderMessages` (ADR 0010 rule 10).
pub fn emit_older_history_loaded(
    events: &EventTx,
    protocol: ProtocolId,
    conversation_id: impl Into<String>,
    before_message_id: impl Into<String>,
    more: bool,
    note: Option<String>,
    topic: Option<MessageTopic>,
) {
    let _ = events.send(AdapterEvent::OlderHistoryLoaded {
        protocol,
        conversation_id: conversation_id.into(),
        before_message_id: before_message_id.into(),
        more,
        note,
        topic,
    });
}

#[cfg_attr(not(feature = "telegram-tdlib"), allow(dead_code))]
pub fn emit_chat_list_loaded(events: &EventTx, protocol: ProtocolId) {
    let _ = events.send(AdapterEvent::ChatListLoaded { protocol });
}

#[cfg_attr(not(feature = "telegram-tdlib"), allow(dead_code))]
pub fn emit_history_loaded(
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
            AdapterEvent::AccountEnded {
                protocol: ProtocolId::WhatsApp
            }
            .inbox_protocol(),
            None,
            "it can come after the session ended"
        );
        assert_eq!(
            AdapterEvent::Stopped {
                protocol: ProtocolId::Telegram
            }
            .inbox_protocol(),
            None
        );
    }
}

#[cfg(test)]
mod plain_words_tests {
    use super::*;

    /// Library and protocol jargon that the account row must not show
    /// (ROADMAP Telegram follow-up).
    const JARGON: [&str; 5] = ["TDLib", "OAuth", "stubbed", "twilight", "morphism"];

    #[test]
    fn account_row_words_have_no_jargon() {
        let statuses = [
            AdapterStatus::Stubbed,
            AdapterStatus::Connecting,
            AdapterStatus::Ready,
            AdapterStatus::Refused,
            AdapterStatus::Error,
        ];
        let catalog = crate::catalog();
        let words = catalog
            .iter()
            .map(|caps| caps.short_label)
            .chain(statuses.iter().map(|status| status.plain_words()));
        for text in words {
            for jargon in JARGON {
                assert!(!text.contains(jargon), "{text:?} contains {jargon:?}");
            }
        }
    }
}
