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

/// The messaging services. Signal runs in its helper process and needs
/// feature `signal-local`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ProtocolKind {
    Telegram,
    WhatsApp,
    Discord,
    Slack,
    Signal,
}

impl ProtocolKind {
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

    /// The lowercase name of settings keys and conversation ids.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Telegram => "telegram",
            Self::WhatsApp => "whatsapp",
            Self::Discord => "discord",
            Self::Slack => "slack",
            Self::Signal => "signal",
        }
    }
}

/// Highest account number of one protocol. Account 0 is the first account.
pub const MAX_ACCOUNT_SLOT: u8 = 8;

/// One account of one protocol: the protocol and the account number.
///
/// Account 0 is the account that thinwire had before it had more than one.
/// It keeps its keychain names, folders, and settings keys. Account `n` of
/// a protocol uses names with `#n` (ADR 0014). The constants
/// `ProtocolId::Telegram` and the others are account 0.
///
/// An adapter sees account 0 only: the host gives each account its own
/// adapter and tags the events of that adapter with the account.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProtocolId {
    kind: ProtocolKind,
    slot: u8,
}

#[allow(non_upper_case_globals)]
impl ProtocolId {
    pub const Telegram: Self = Self::primary(ProtocolKind::Telegram);
    pub const WhatsApp: Self = Self::primary(ProtocolKind::WhatsApp);
    pub const Discord: Self = Self::primary(ProtocolKind::Discord);
    pub const Slack: Self = Self::primary(ProtocolKind::Slack);
    pub const Signal: Self = Self::primary(ProtocolKind::Signal);

    /// Account 0 of each protocol.
    pub const ALL: [Self; 5] = [
        Self::Telegram,
        Self::WhatsApp,
        Self::Discord,
        Self::Slack,
        Self::Signal,
    ];

    /// Account 0 of `kind`.
    #[must_use]
    pub const fn primary(kind: ProtocolKind) -> Self {
        Self { kind, slot: 0 }
    }

    /// Account `slot` of `kind`. `None` above [`MAX_ACCOUNT_SLOT`].
    #[must_use]
    pub const fn account(kind: ProtocolKind, slot: u8) -> Option<Self> {
        if slot > MAX_ACCOUNT_SLOT {
            None
        } else {
            Some(Self { kind, slot })
        }
    }

    #[must_use]
    pub const fn kind(self) -> ProtocolKind {
        self.kind
    }

    /// The account number. 0 is the first account.
    #[must_use]
    pub const fn slot(self) -> u8 {
        self.slot
    }

    /// Account 0 of the same protocol. An adapter sees this id.
    #[must_use]
    pub const fn first_account(self) -> Self {
        Self::primary(self.kind)
    }

    /// The same protocol, account or not.
    #[must_use]
    pub const fn is(self, kind: ProtocolKind) -> bool {
        self.kind as u8 == kind as u8
    }

    /// The same protocol as `other`, in any account.
    #[must_use]
    pub const fn same_protocol(self, other: Self) -> bool {
        self.kind as u8 == other.kind as u8
    }

    /// The protocol name, the same for every account.
    #[must_use]
    pub const fn display_name(self) -> &'static str {
        self.kind.display_name()
    }

    /// The protocol name, with the account number after account 0:
    /// "Telegram", "Telegram 2".
    #[must_use]
    pub fn label(self) -> String {
        if self.slot == 0 {
            self.display_name().to_owned()
        } else {
            format!("{} {}", self.display_name(), u16::from(self.slot) + 1)
        }
    }

    /// The key of settings and keychain names: "telegram", "telegram#1".
    #[must_use]
    pub fn key(self) -> String {
        if self.slot == 0 {
            self.kind.key().to_owned()
        } else {
            format!("{}#{}", self.kind.key(), self.slot)
        }
    }

    /// Read [`Self::key`] back. `None` for another text.
    #[must_use]
    pub fn from_key(text: &str) -> Option<Self> {
        let (name, slot) = match text.split_once('#') {
            Some((name, slot)) => (name, slot.parse::<u8>().ok().filter(|slot| *slot > 0)?),
            None => (text, 0),
        };
        let kind = ProtocolKind::ALL
            .into_iter()
            .find(|kind| kind.key() == name)?;
        Self::account(kind, slot)
    }

    /// A name for an account in the keychain or on disk: `name` for account
    /// 0, `name#n` for account `n`.
    #[must_use]
    pub fn scoped(self, name: &str) -> String {
        if self.slot == 0 {
            name.to_owned()
        } else {
            format!("{name}#{}", self.slot)
        }
    }

    /// The inbox downloads a chat photo for this protocol.
    /// The other protocols draw initials.
    #[must_use]
    pub const fn downloads_avatar(self) -> bool {
        self.is(ProtocolKind::Telegram)
    }
}

impl fmt::Debug for ProtocolId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.slot == 0 {
            write!(f, "{:?}", self.kind)
        } else {
            write!(f, "{:?}#{}", self.kind, self.slot)
        }
    }
}

impl fmt::Display for ProtocolId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label())
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
    /// The adapter answers `SendMedia`. The compose field takes photos and
    /// videos only for such a protocol.
    pub sends_media: bool,
    /// What the message menu offers for this protocol.
    pub message_actions: MessageActions,
    /// The adapter answers `SetTyping` and sends `Typing` events (#510).
    pub typing: bool,
}

/// What a user can do with one message, by protocol. The message menu shows
/// only these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MessageActions {
    /// `SendText` takes `reply_to`.
    pub reply: bool,
    /// The adapter answers `React` and sends `MessageReactions`.
    pub react: bool,
    /// `DeleteMessage` with `for_everyone: false` removes the message for
    /// this account only.
    pub delete_for_me: bool,
    /// `DeleteMessage` with `for_everyone: true` removes an own message for
    /// every member of the chat.
    pub delete_for_everyone: bool,
    /// A new reaction of this account takes the place of its old one.
    pub one_reaction: bool,
    /// `EditMessage` changes the text of an own text message.
    pub edit: bool,
    /// `ForwardMessages` sends one or more messages, with their media, to
    /// another chat of the same account.
    pub forward: bool,
}

impl MessageActions {
    /// A protocol with no message action.
    pub const NONE: Self = Self {
        reply: false,
        react: false,
        delete_for_me: false,
        delete_for_everyone: false,
        one_reaction: false,
        edit: false,
        forward: false,
    };

    /// Every action, with one reaction for each person, as Telegram (with
    /// no Premium), WhatsApp, and Signal count them.
    pub const ALL: Self = Self {
        reply: true,
        react: true,
        delete_for_me: true,
        delete_for_everyone: true,
        one_reaction: true,
        edit: true,
        forward: true,
    };
}

impl Reaction {
    /// Add or remove the reaction `emoji` of this account in `reactions`, as
    /// the protocol will count it. `one_reaction` removes the old reaction of
    /// this account first.
    pub fn toggle(reactions: &mut Vec<Self>, emoji: &str, add: bool, one_reaction: bool) {
        if add && one_reaction {
            for reaction in reactions.iter_mut() {
                if reaction.mine && reaction.emoji != emoji {
                    reaction.mine = false;
                    reaction.count = reaction.count.saturating_sub(1);
                }
            }
        }
        match reactions
            .iter_mut()
            .find(|reaction| reaction.emoji == emoji)
        {
            Some(reaction) if add && !reaction.mine => {
                reaction.mine = true;
                reaction.count = reaction.count.saturating_add(1);
            }
            Some(reaction) if !add && reaction.mine => {
                reaction.mine = false;
                reaction.count = reaction.count.saturating_sub(1);
            }
            Some(_) => {}
            None if add => reactions.push(Self {
                emoji: emoji.to_owned(),
                count: 1,
                mine: true,
            }),
            None => {}
        }
        reactions.retain(|reaction| reaction.count > 0);
    }
}

/// One emoji on a message, with the number of people who chose it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reaction {
    pub emoji: String,
    pub count: u32,
    /// This account chose it.
    pub mine: bool,
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
    /// `store_cursor` is an exclusive store timestamp. It is not a message id.
    /// `None` reads before `before_message_id`.
    LoadOlderMessages {
        protocol: ProtocolId,
        conversation_id: String,
        before_message_id: String,
        store_cursor: Option<u64>,
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
        /// The message this one replies to. `None` is a plain message.
        reply_to: Option<String>,
    },
    /// Send photos and videos, with the text as the caption. The caption can
    /// be empty. `topic` is the open topic, if any. A protocol that takes
    /// fewer files in one message sends more than one message. The adapter
    /// answers one `SendAccepted` or `SendRejected` with `request`.
    SendMedia {
        protocol: ProtocolId,
        conversation_id: String,
        topic: Option<MessageTopic>,
        files: Vec<crate::media::OutgoingFile>,
        caption: String,
        request: u64,
    },
    /// Send plain text in one topic. The body is the user's message.
    SendTopicText {
        protocol: ProtocolId,
        conversation_id: String,
        topic: MessageTopic,
        body: String,
        request: u64,
        /// The message this one replies to. `None` is a plain message.
        reply_to: Option<String>,
    },
    /// The host starts the adapter of this account (ADR 0014). Account 0 of
    /// each protocol starts with the host. No adapter sees this command.
    AddAccount {
        protocol: ProtocolId,
    },
    /// The host shuts the adapter of this account down and drops it. Its
    /// data stays. No adapter sees this command.
    RemoveAccount {
        protocol: ProtocolId,
    },
    /// `command` for the adapter of `account`. The core wraps a command with
    /// no protocol field (a login step, a pairing) for an account other than
    /// account 0. The host unwraps it.
    ToAccount {
        account: ProtocolId,
        command: Box<AdapterCommand>,
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
    /// Read the session key again and open the saved session. This does not
    /// start a link and it does not delete a store. A wrong key tries the
    /// move-aside again.
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
    /// Download one Telegram voice file. The file stays on disk. The worker
    /// reports a path and never reads the bytes into this process.
    DownloadVoice {
        protocol: ProtocolId,
        file_id: i32,
    },
    /// Delete one message. `for_everyone` removes an own message for every
    /// member, where the protocol allows it. The adapter answers with
    /// `MessagesRemoved`, or with `CommandFailed`.
    DeleteMessage {
        protocol: ProtocolId,
        conversation_id: String,
        message_id: String,
        for_everyone: bool,
    },
    /// Add or remove the reaction `emoji` of this account on one message.
    React {
        protocol: ProtocolId,
        conversation_id: String,
        message_id: String,
        emoji: String,
        add: bool,
    },
    /// Put `body` in the place of the text of one own message. The adapter
    /// answers with `MessageBody` or `MessageMedia` and `MessageEdited`, or
    /// with `CommandFailed`.
    EditMessage {
        protocol: ProtocolId,
        conversation_id: String,
        message_id: String,
        body: String,
    },
    /// Forward the messages `message_ids` of `from_conversation_id`, with
    /// their media, to `to_conversation_id` of the same account. The new
    /// messages come as `Message` events of that chat. The adapter answers
    /// with `Notice`, or with `CommandFailed` when nothing was forwarded.
    ForwardMessages {
        protocol: ProtocolId,
        from_conversation_id: String,
        message_ids: Vec<String>,
        to_conversation_id: String,
    },
    /// Download one video file for the in-app player (ADR 0015). The file
    /// stays on disk. The adapter reports a path.
    DownloadVideo {
        protocol: ProtocolId,
        file_id: i32,
    },
    /// Download one document. The file stays on disk. The adapter reports
    /// a path and never reads the bytes into this process.
    DownloadDocument {
        protocol: ProtocolId,
        file_id: i32,
    },
    /// Tell the other members of a chat, or of one topic, that this account
    /// types (`typing: true`) or stopped (`typing: false`). The protocol ends
    /// the typing state by itself after some seconds, and when a message of
    /// this account arrives. No answer comes back (#510).
    SetTyping {
        protocol: ProtocolId,
        conversation_id: String,
        topic: Option<MessageTopic>,
        typing: bool,
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
            | Self::SendMedia { protocol, .. }
            | Self::ResendMessage { protocol, .. }
            | Self::ViewChat { protocol, .. }
            | Self::ViewTopic { protocol, .. }
            | Self::RestartHelper { protocol }
            | Self::RetryStoreKey { protocol }
            | Self::AddAccount { protocol }
            | Self::RemoveAccount { protocol }
            | Self::DownloadFile { protocol, .. }
            | Self::DownloadVoice { protocol, .. }
            | Self::DownloadVideo { protocol, .. }
            | Self::DownloadDocument { protocol, .. }
            | Self::DeleteMessage { protocol, .. }
            | Self::EditMessage { protocol, .. }
            | Self::ForwardMessages { protocol, .. }
            | Self::SetTyping { protocol, .. }
            | Self::React { protocol, .. } => protocol,
            Self::ToAccount { account, .. } => account,
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

    /// The command for `account`. A command with a protocol field gets the
    /// account there. Another command, for an account other than 0, goes in
    /// [`Self::ToAccount`].
    #[must_use]
    pub fn for_account(mut self, account: ProtocolId) -> Self {
        if self.set_protocol(account) || account.slot() == 0 {
            self
        } else {
            Self::ToAccount {
                account,
                command: Box::new(self),
            }
        }
    }

    /// The account this command is for, and the command as its adapter
    /// sees it: every protocol field names account 0 (ADR 0014).
    #[must_use]
    pub fn route(self) -> (ProtocolId, Self) {
        let account = self.protocol();
        let mut command = match self {
            Self::ToAccount { command, .. } => *command,
            other => other,
        };
        command.set_protocol(account.first_account());
        (account, command)
    }

    /// Set every protocol field. `false` when the command has none.
    fn set_protocol(&mut self, account: ProtocolId) -> bool {
        match self {
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
            | Self::SendMedia { protocol, .. }
            | Self::ResendMessage { protocol, .. }
            | Self::ViewChat { protocol, .. }
            | Self::ViewTopic { protocol, .. }
            | Self::RestartHelper { protocol }
            | Self::RetryStoreKey { protocol }
            | Self::AddAccount { protocol }
            | Self::RemoveAccount { protocol }
            | Self::DownloadFile { protocol, .. }
            | Self::DownloadVoice { protocol, .. }
            | Self::DownloadVideo { protocol, .. }
            | Self::DownloadDocument { protocol, .. }
            | Self::DeleteMessage { protocol, .. }
            | Self::EditMessage { protocol, .. }
            | Self::ForwardMessages { protocol, .. }
            | Self::SetTyping { protocol, .. }
            | Self::React { protocol, .. } => {
                *protocol = account;
                true
            }
            Self::ToAccount {
                account: target, ..
            } => {
                *target = account;
                true
            }
            Self::ConnectDiscord { .. }
            | Self::TelegramAuth { .. }
            | Self::WhatsAppAcknowledgeRisk
            | Self::WhatsAppBeginLink { .. }
            | Self::WhatsAppCancelLink
            | Self::SignalAcknowledgeNotice
            | Self::SignalBeginLink { .. }
            | Self::SignalCancelLink => false,
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
    /// The store files could not be moved aside. They stay. Nothing was deleted.
    MoveFailed,
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
    /// The sender changed the text of a message after the send. The new text
    /// comes in `MessageBody` or `MessageMedia`. The thread shows "edited".
    MessageEdited {
        protocol: ProtocolId,
        conversation_id: String,
        message_id: String,
    },
    /// Replace the caption, the links, and the media on a message.
    /// A text edit sets `photo`, `audio`, `video`, and `file` to `None`.
    MessageMedia {
        protocol: ProtocolId,
        conversation_id: String,
        message_id: String,
        body: String,
        links: Vec<crate::media::MessageLink>,
        photo: Option<crate::media::MessagePhoto>,
        audio: Option<crate::media::MessageAudio>,
        video: Option<crate::media::MessageVideo>,
        file: Option<crate::media::MessageFile>,
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
    VoiceReady {
        protocol: ProtocolId,
        file_id: i32,
        path: crate::media::AudioPath,
    },
    VoiceFailed {
        protocol: ProtocolId,
        file_id: i32,
    },
    /// The reactions of one message now. An empty list has none.
    MessageReactions {
        protocol: ProtocolId,
        conversation_id: String,
        message_id: String,
        reactions: Vec<Reaction>,
    },
    /// The small picture of a chat, as image bytes: a contact or a group
    /// picture. It changes only the picture of that row.
    ChatAvatar {
        protocol: ProtocolId,
        conversation_id: String,
        bytes: crate::media::ImageBytes,
    },
    /// The video `file_id` is a local file now.
    VideoReady {
        protocol: ProtocolId,
        file_id: i32,
        path: crate::media::AudioPath,
    },
    VideoFailed {
        protocol: ProtocolId,
        file_id: i32,
    },
    FileReady {
        protocol: ProtocolId,
        file_id: i32,
        path: crate::media::AudioPath,
    },
    FileFailed {
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
    /// The other party has read outgoing messages through this id.
    /// Telegram `last_read_outbox_message_id` and `updateChatReadOutbox`.
    /// The id is the protocol message id, not the app's `telegram:chat:id` string.
    OutboxRead {
        protocol: ProtocolId,
        conversation_id: String,
        last_read_message_id: i64,
    },
    /// A chat-list page load ended (loaded, already complete, or failed).
    /// The UI stops its "Loading chats…" state.
    ChatListLoaded {
        protocol: ProtocolId,
    },
    /// A person started (`typing: true`) or stopped typing in a chat, or in
    /// one topic of a chat. `sender_id` tells one person from another.
    /// `sender` is the name to show. The core shows the person as typing
    /// for some seconds after the last event (#510).
    Typing {
        protocol: ProtocolId,
        conversation_id: String,
        topic: Option<MessageTopic>,
        sender_id: String,
        sender: String,
        typing: bool,
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
        /// Store position for the next page, and how many rows this page
        /// could not read. `None` when this page skipped nothing and the
        /// next page can use `before_message_id`.
        store_cursor: Option<HistoryStoreCursor>,
    },
    /// A history load for one chat ended. The UI stops "Loading messages…".
    HistoryLoaded {
        protocol: ProtocolId,
        conversation_id: String,
        /// Set when the first page stopped on rows it could not read.
        store_cursor: Option<HistoryStoreCursor>,
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
    /// `event` of the adapter of `account`, for an event with no protocol
    /// field (a login step, a QR code) of an account other than 0. The host
    /// wraps it (ADR 0014).
    FromAccount {
        account: ProtocolId,
        event: Box<AdapterEvent>,
    },
}

impl AdapterEvent {
    /// The event of the adapter of `account`: every protocol field names the
    /// account. An event with no protocol field, of an account other than 0,
    /// goes in [`Self::FromAccount`]. The host calls this for each event.
    #[must_use]
    pub fn for_account(mut self, account: ProtocolId) -> Self {
        if self.set_account(account) || account.slot() == 0 {
            self
        } else {
            Self::FromAccount {
                account,
                event: Box::new(self),
            }
        }
    }

    /// The account of the event and the event, with any
    /// [`Self::FromAccount`] removed. `None` for an event with no protocol
    /// field: that is account 0 of its protocol.
    #[must_use]
    pub fn from_account(self) -> (Option<ProtocolId>, Self) {
        match self {
            Self::FromAccount { account, event } => (Some(account), *event),
            other => (None, other),
        }
    }

    /// Set every protocol field, also inside a chat, a topic, or a message.
    /// `false` when the event has none.
    fn set_account(&mut self, account: ProtocolId) -> bool {
        match self {
            Self::Status { protocol, .. }
            | Self::StoreAccess { protocol, .. }
            | Self::AccountEnded { protocol }
            | Self::ConversationRemoved { protocol, .. }
            | Self::MessageBody { protocol, .. }
            | Self::MessageEdited { protocol, .. }
            | Self::MessageMedia { protocol, .. }
            | Self::MessageLinks { protocol, .. }
            | Self::PhotoReady { protocol, .. }
            | Self::PhotoFailed { protocol, .. }
            | Self::VoiceReady { protocol, .. }
            | Self::VoiceFailed { protocol, .. }
            | Self::VideoReady { protocol, .. }
            | Self::VideoFailed { protocol, .. }
            | Self::FileReady { protocol, .. }
            | Self::FileFailed { protocol, .. }
            | Self::ChatAvatar { protocol, .. }
            | Self::MessageReactions { protocol, .. }
            | Self::MessagesRemoved { protocol, .. }
            | Self::SendAccepted { protocol, .. }
            | Self::SendRejected { protocol, .. }
            | Self::Stopped { protocol }
            | Self::Account { protocol, .. }
            | Self::CommandFailed { protocol, .. }
            | Self::Notice { protocol, .. }
            | Self::MessageDelivery { protocol, .. }
            | Self::OutboxRead { protocol, .. }
            | Self::ChatListLoaded { protocol }
            | Self::Typing { protocol, .. }
            | Self::OlderHistoryLoaded { protocol, .. }
            | Self::HistoryLoaded { protocol, .. }
            | Self::Helper { protocol, .. } => {
                *protocol = account;
                true
            }
            Self::ConversationUpsert { conversation } => {
                conversation.protocol = account;
                true
            }
            Self::TopicUpsert { topic } => {
                topic.protocol = account;
                true
            }
            Self::MessageReceived { message } => {
                message.protocol = account;
                true
            }
            Self::MessageReplaced {
                protocol, message, ..
            } => {
                *protocol = account;
                message.protocol = account;
                true
            }
            Self::FromAccount {
                account: source, ..
            } => {
                *source = account;
                true
            }
            // A login event goes in `FromAccount` as a whole, so the epoch
            // check of the host still sees `Login` first.
            Self::Login { .. }
            | Self::TelegramAuth { .. }
            | Self::TelegramAuthRejected { .. }
            | Self::TelegramDataReset { .. }
            | Self::TelegramSessionEnded
            | Self::TelegramCodeSent { .. }
            | Self::FlushSecrets
            | Self::WhatsAppQr { .. }
            | Self::WhatsAppPairCode { .. }
            | Self::SignalQr { .. } => false,
        }
    }

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
            | Self::MessageEdited { protocol, .. }
            | Self::MessageMedia { protocol, .. }
            | Self::MessageLinks { protocol, .. }
            | Self::PhotoReady { protocol, .. }
            | Self::PhotoFailed { protocol, .. }
            | Self::VoiceReady { protocol, .. }
            | Self::VoiceFailed { protocol, .. }
            | Self::VideoReady { protocol, .. }
            | Self::VideoFailed { protocol, .. }
            | Self::FileReady { protocol, .. }
            | Self::FileFailed { protocol, .. }
            | Self::ChatAvatar { protocol, .. }
            | Self::MessageReactions { protocol, .. }
            | Self::MessagesRemoved { protocol, .. }
            | Self::SendAccepted { protocol, .. }
            | Self::SendRejected { protocol, .. }
            | Self::MessageDelivery { protocol, .. }
            | Self::OutboxRead { protocol, .. }
            | Self::ChatListLoaded { protocol }
            | Self::Typing { protocol, .. }
            | Self::HistoryLoaded { protocol, .. }
            | Self::OlderHistoryLoaded { protocol, .. } => Some(*protocol),
            _ => None,
        }
    }
}

/// Most unreadable runs one store cursor may carry.
///
/// One history page scans at most 208 rows, so a cursor cannot honestly name
/// more runs than that. The helper decode drops a longer list.
pub const MAX_HISTORY_CURSOR_RUNS: usize = 208;

/// One unreadable stretch on a history page, and the shown message it sits
/// older than.
///
/// `after_id` is that message's id. `None` means the stretch is newer than
/// every row this page shows, so the core places it at the request anchor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistorySkipRun {
    pub after_id: Option<String>,
    pub skipped: u32,
}

/// Where the next Signal history read starts, and how many rows this page
/// could not read.
///
/// `before_ms` is an exclusive store timestamp. It is not a shown message id.
/// The skip count and the position of each stretch are part of this value.
/// The wire adds this one field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryStoreCursor {
    pub before_ms: u64,
    /// Rows in this page that the store could not read.
    pub skipped: u32,
    /// The next read starts at `before_ms`. `false` means the next read still
    /// uses the oldest shown message id. The count is still shown.
    pub resume: bool,
    /// Where those rows sit among the rows this page shows. When `runs` is
    /// empty, the core draws one line for `skipped` at the oldest edge of
    /// the page.
    pub runs: Vec<HistorySkipRun>,
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
    /// The other party has read this outgoing message.
    Read,
}

/// What a Telegram update says about one outgoing message.
///
/// `updateMessageSendSucceeded` and `updateMessageSendFailed` are the result.
/// Their message object can still carry a pending `sending_state`. The result
/// decides the row. A listed message (a new message, history, or the chat's
/// last message) uses `sending`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutgoingUpdate {
    /// A message already stored or just inserted. `sending` is its state.
    Listed,
    /// `updateMessageSendSucceeded`. The server has the message.
    Succeeded,
    /// `updateMessageSendFailed`.
    Failed,
}

/// Delivery the thread shows for one Telegram message.
///
/// A succeeded update is sent, then read when `message_id` is at or below
/// `last_read_outbox_message_id`. A failed update is failed. A pending or
/// failed message is not read. An incoming message is sent.
#[must_use]
pub fn outgoing_delivery(
    update: OutgoingUpdate,
    sending: Delivery,
    outgoing: bool,
    message_id: i64,
    last_read_outbox_message_id: i64,
) -> Delivery {
    if !outgoing {
        return Delivery::Sent;
    }
    let delivery = match update {
        OutgoingUpdate::Listed => sending,
        OutgoingUpdate::Succeeded => Delivery::Sent,
        OutgoingUpdate::Failed => Delivery::Failed,
    };
    with_outbox_read(delivery, message_id, last_read_outbox_message_id)
}

/// `Sent` becomes `Read` when the other party has read through `message_id`.
/// Pending and failed stay as they are. A zero cursor has read nothing.
#[must_use]
pub fn with_outbox_read(
    delivery: Delivery,
    message_id: i64,
    last_read_outbox_message_id: i64,
) -> Delivery {
    if delivery == Delivery::Sent
        && last_read_outbox_message_id > 0
        && message_id > 0
        && message_id <= last_read_outbox_message_id
    {
        Delivery::Read
    } else {
        delivery
    }
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
    pub audio: Option<crate::media::MessageAudio>,
    /// A video clip. `photo` is its first picture, when the protocol sends one.
    pub video: Option<crate::media::MessageVideo>,
    /// A document. [`Self::body`] is the caption, and it can be empty.
    pub file: Option<crate::media::MessageFile>,
    /// The reactions on the message, as the protocol counts them.
    pub reactions: Vec<Reaction>,
    /// The id of the message that this one replies to. The shell finds its
    /// sender and text in the open thread.
    pub reply_to: Option<String>,
    /// The sender changed the text after the send.
    pub edited: bool,
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
pub(crate) fn emit_message_edited(
    events: &EventTx,
    protocol: ProtocolId,
    conversation_id: impl Into<String>,
    message_id: impl Into<String>,
) {
    let _ = events.send(AdapterEvent::MessageEdited {
        protocol,
        conversation_id: conversation_id.into(),
        message_id: message_id.into(),
    });
}

#[cfg_attr(not(feature = "telegram-tdlib"), allow(dead_code))]
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_message_media(
    events: &EventTx,
    protocol: ProtocolId,
    conversation_id: impl Into<String>,
    message_id: impl Into<String>,
    body: impl Into<String>,
    links: Vec<crate::media::MessageLink>,
    photo: Option<crate::media::MessagePhoto>,
    audio: Option<crate::media::MessageAudio>,
    video: Option<crate::media::MessageVideo>,
    file: Option<crate::media::MessageFile>,
) {
    let _ = events.send(AdapterEvent::MessageMedia {
        protocol,
        conversation_id: conversation_id.into(),
        message_id: message_id.into(),
        body: body.into(),
        links,
        photo,
        audio,
        video,
        file,
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
pub(crate) fn emit_outbox_read(
    events: &EventTx,
    protocol: ProtocolId,
    conversation_id: impl Into<String>,
    last_read_message_id: i64,
) {
    let _ = events.send(AdapterEvent::OutboxRead {
        protocol,
        conversation_id: conversation_id.into(),
        last_read_message_id,
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
        store_cursor: None,
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
        store_cursor: None,
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
    fn a_reaction_toggles_and_one_reaction_moves_the_old_one() {
        let mut reactions = vec![Reaction {
            emoji: "👍".into(),
            count: 2,
            mine: false,
        }];
        Reaction::toggle(&mut reactions, "👍", true, true);
        assert_eq!(reactions[0].count, 3);
        assert!(reactions[0].mine);
        Reaction::toggle(&mut reactions, "👍", true, true);
        assert_eq!(reactions[0].count, 3, "a second add changes nothing");
        Reaction::toggle(&mut reactions, "❤️", true, true);
        assert_eq!(
            reactions,
            vec![
                Reaction {
                    emoji: "👍".into(),
                    count: 2,
                    mine: false
                },
                Reaction {
                    emoji: "❤️".into(),
                    count: 1,
                    mine: true
                },
            ]
        );
        Reaction::toggle(&mut reactions, "❤️", false, true);
        assert_eq!(reactions.len(), 1, "a count of 0 leaves the list");
        Reaction::toggle(&mut reactions, "😂", true, false);
        Reaction::toggle(&mut reactions, "👍", true, false);
        assert!(
            reactions.iter().all(|reaction| reaction.mine),
            "many reactions"
        );
    }

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

#[cfg(test)]
mod account_tests {
    use super::*;

    fn second(kind: ProtocolKind) -> ProtocolId {
        ProtocolId::account(kind, 1).expect("slot 1")
    }

    /// Account 0 keeps its old names. Another account adds its number.
    #[test]
    fn an_account_has_a_key_a_label_and_scoped_names() {
        assert_eq!(ProtocolId::Telegram.key(), "telegram");
        assert_eq!(ProtocolId::Telegram.label(), "Telegram");
        assert_eq!(
            ProtocolId::Telegram.scoped("telegram.session"),
            "telegram.session"
        );
        let other = second(ProtocolKind::WhatsApp);
        assert_eq!(other.key(), "whatsapp#1");
        assert_eq!(other.label(), "WhatsApp 2");
        assert_eq!(other.scoped("whatsapp.store_key"), "whatsapp.store_key#1");
        assert_eq!(format!("{other:?}"), "WhatsApp#1");
        assert_eq!(ProtocolId::from_key("whatsapp#1"), Some(other));
        assert_eq!(ProtocolId::from_key("slack"), Some(ProtocolId::Slack));
        assert_eq!(
            ProtocolId::from_key("slack#0"),
            None,
            "account 0 has no number"
        );
        assert_eq!(ProtocolId::from_key("slack#9"), None, "above the limit");
        assert_eq!(ProtocolId::from_key("irc"), None);
        assert!(other.is(ProtocolKind::WhatsApp));
        assert!(other.same_protocol(ProtocolId::WhatsApp));
        assert_ne!(other, ProtocolId::WhatsApp);
        assert_eq!(other.first_account(), ProtocolId::WhatsApp);
        assert!(ProtocolId::account(ProtocolKind::Signal, MAX_ACCOUNT_SLOT).is_some());
        assert!(ProtocolId::account(ProtocolKind::Signal, MAX_ACCOUNT_SLOT + 1).is_none());
    }

    /// A command for another account names it, and its adapter sees
    /// account 0. A login step with no protocol field goes in `ToAccount`.
    #[test]
    fn a_command_routes_to_its_account_and_the_adapter_sees_account_0() {
        let account = second(ProtocolKind::Telegram);
        let open = AdapterCommand::OpenChat {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
        }
        .for_account(account);
        assert_eq!(open.protocol(), account);
        let (to, seen) = open.route();
        assert_eq!(to, account);
        assert_eq!(seen.protocol(), ProtocolId::Telegram);

        let step = AdapterCommand::TelegramAuth {
            step: TelegramAuthStep::Phone,
            epoch: 3,
        };
        let wrapped = step.clone().for_account(account);
        assert!(matches!(wrapped, AdapterCommand::ToAccount { .. }));
        assert_eq!(wrapped.protocol(), account);
        let (to, seen) = wrapped.route();
        assert_eq!(to, account);
        assert_eq!(seen, step);
        // Account 0 needs no wrapper.
        assert_eq!(step.clone().for_account(ProtocolId::Telegram), step);
    }

    /// The host tags each event of an adapter with its account, also inside
    /// a chat row. An event with no protocol field goes in `FromAccount`.
    #[test]
    fn an_event_gets_its_account() {
        let account = second(ProtocolKind::Telegram);
        let message = ChatMessage {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            id: "telegram:1:1".into(),
            sender: "Ana".into(),
            body: "hi".into(),
            outbound: false,
            delivery: Delivery::Sent,
            sent_at: 1,
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
        };
        let AdapterEvent::MessageReplaced {
            protocol, message, ..
        } = (AdapterEvent::MessageReplaced {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            old_id: "old".into(),
            message,
        })
        .for_account(account)
        else {
            panic!("a replace stays a replace");
        };
        assert_eq!(protocol, account);
        assert_eq!(message.protocol, account);

        let phase = AdapterEvent::TelegramAuth {
            phase: TelegramAuthPhase::Ready,
        };
        let (from, inner) = phase.clone().for_account(account).from_account();
        assert_eq!(from, Some(account));
        assert_eq!(inner, phase);
        let (from, inner) = phase
            .clone()
            .for_account(ProtocolId::Telegram)
            .from_account();
        assert_eq!(from, None, "account 0 needs no wrapper");
        assert_eq!(inner, phase);
    }
}
