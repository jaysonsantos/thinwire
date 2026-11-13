//! What a frontend asks the core to do.
//!
//! Frontends send intents, never `AdapterCommand` values. The core checks
//! each intent against its state, then queues commands for the worker.
//! Each protocol has its own sub-enum, so protocol work does not collide on
//! one match. Typed secrets use [`SecretText`], so `Debug` stays clean.

use crate::notify::NotifyKey;
use thinwire_protocol::{MessageTopic, ProtocolId};

use crate::state::AuthKey;
use crate::{SecretText, ThemeMode};

/// One user action. Frontends map clicks and keys to these values.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Intent {
    /// Show the accounts and chats of one protocol.
    SelectProtocol(ProtocolId),
    /// Add an account of this protocol and open its login screen. Account 0
    /// is used while it is free: not linked and not in a login. Else the
    /// lowest free account number (ADR 0014).
    AddAccount(thinwire_protocol::ProtocolKind),
    /// Open the login screen of this account again, for example after its
    /// session ended.
    LinkAccount(ProtocolId),
    /// Remove an account after account 0 that is not linked. Its keychain
    /// values and files stay.
    RemoveAccount(ProtocolId),
    /// Unlink a linked account: log out where the protocol has a log out,
    /// and remove the saved session or the saved token. A frontend asks the
    /// user first. Account 0 stays as a row that is not linked. An account
    /// after account 0 leaves the sidebar and the settings.
    UnlinkAccount(ProtocolId),
    /// Show one previous Signal account. The slot is not an account id.
    /// Sending stays off. The history stays.
    SelectRetainedSignal(u32),
    /// Open a chat of the selected protocol and load its recent messages.
    SelectConversation { id: String },
    /// Open one topic of a chat. The chat id is named so a click in the
    /// same frame cannot apply to another chat.
    SelectTopic {
        conversation_id: String,
        topic: MessageTopic,
    },
    /// Leave the open topic of this chat and show its topic list. Dropped
    /// when another chat is open.
    CloseTopic { conversation_id: String },
    /// Move the inbox highlight by `delta` rows. Down is positive.
    /// This does not open the chat and does not focus the compose field.
    MoveInbox { delta: i32 },
    /// Move the inbox highlight to the row that has keyboard focus.
    /// This does not open the chat.
    FocusInbox { id: String },
    /// Inbox search text. Matches chat title and participant only.
    /// `SecretText` keeps the typed text out of `Debug`.
    SetSearch(SecretText),
    /// Reload the chat lists of the visible protocols.
    Refresh,
    /// Close the error block ("What happened / Why / What to do").
    DismissError,
    /// Enter or Escape on the center screen: first run or the login form.
    Key(AuthKey),
    /// Unsent text of one chat. It names the chat it was typed in, so a
    /// selection change in the same frame cannot move the text to another
    /// chat (PR #48 review). `SecretText` keeps the text out of `Debug`.
    SetDraft {
        protocol: ProtocolId,
        conversation_id: String,
        topic: Option<MessageTopic>,
        text: SecretText,
    },
    /// Send the draft of this chat. Dropped when the chat is no longer the
    /// selected one, so it never goes to another chat.
    SendDraft {
        protocol: ProtocolId,
        conversation_id: String,
        topic: Option<MessageTopic>,
    },
    /// Attach photos and videos to the tray of this thread: dropped files,
    /// pasted files, or a pasted image that the frontend saved as a file.
    /// Another file is skipped, and the tray says why.
    AddAttachments {
        protocol: ProtocolId,
        conversation_id: String,
        topic: Option<MessageTopic>,
        paths: Vec<std::path::PathBuf>,
    },
    /// Take the file at `index` out of the tray of this thread.
    RemoveAttachment {
        protocol: ProtocolId,
        conversation_id: String,
        topic: Option<MessageTopic>,
        index: usize,
    },
    /// Send a failed outgoing message again.
    Retry { message_id: String },
    /// The thread of this chat reached the top: load the page before its
    /// oldest message. Dropped when the chat is no longer the selected one.
    /// The core sends one request at a time for each chat (#30).
    LoadOlderMessages {
        protocol: ProtocolId,
        conversation_id: String,
        topic: Option<MessageTopic>,
    },
    /// Read the OS keychain again after a failed read. The read runs off the caller thread.
    RetryKeychain,
    /// Store the light/dark preference. Disk I/O runs off the caller thread.
    SetTheme(ThemeMode),
    /// Close every client cleanly. The view reports when all stopped.
    Shutdown,
    /// The app window gained or lost focus. A chat that is open in a window
    /// without focus still notifies (#32).
    WindowFocus(bool),
    /// The user clicked a notification: open that chat (#32).
    OpenFromNotification(NotifyKey),
    /// Settings: desktop notifications on or off (#32).
    SetNotifications(bool),
    /// Settings: show the sender and the text in notifications (#32).
    SetNotificationPreview(bool),
    /// Collapse the account sidebar to its icons, or expand it. Disk I/O
    /// runs off the caller thread.
    SetRailCollapsed(bool),
    /// Split view: show this account in a second pane, on the right, and
    /// give that pane the focus. Each pane has its own chat list and thread.
    OpenSplit(ProtocolId),
    /// Split view: go back to one pane. The pane with the focus stays.
    CloseSplit,
    /// Split view: give the focus to the other pane.
    FocusOtherPane,
    /// Move one account one place up or down in the account sidebar. The
    /// order is saved. Disk I/O runs off the caller thread.
    MoveAccount { account: ProtocolId, up: bool },
    /// Settings: tell the other people of a chat when the user types
    /// (#510). Off also stops a typing state that still shows. Disk I/O
    /// runs off the caller thread.
    SetSendTyping(bool),
    /// Settings: one bubble color for each other person of a chat (#581),
    /// or one color for all of them. Disk I/O runs off the caller thread.
    SetPersonColors(bool),
    /// The user picked this emoji in the emoji picker (#515): it goes first
    /// in the recent emoji. The reaction is a separate [`Self::React`]. Disk
    /// I/O runs off the caller thread.
    UseEmoji(String),
    /// Mute or unmute one chat in thinwire (#153). A protocol mute wins:
    /// an unmute here never unmutes it. Disk I/O runs off the caller thread.
    SetChatMute {
        protocol: ProtocolId,
        conversation_id: String,
        muted: bool,
    },
    /// Start the helper process of this protocol again (ADR 0013). It acts
    /// only while the account row shows "Helper stopped."
    RestartHelper(ProtocolId),
    /// Read the session key again after the keychain refused the read.
    /// This does not start a link and does not move the store.
    RetryStoreKey(ProtocolId),
    /// Open the README risk text for an unofficial account. This does not
    /// start or cancel a link.
    ShowNotice(ProtocolId),
    /// Close the read-only risk text.
    CloseNotice,
    /// Download one photo that is on screen. The core sends one request
    /// until that file finishes or fails.
    LoadPhoto {
        protocol: ProtocolId,
        conversation_id: String,
        message_id: String,
        file_id: i32,
    },
    /// Download the small photo of one inbox row. The core sends one request
    /// until that file finishes or fails. The same download path as [`Self::LoadPhoto`].
    LoadAvatar {
        protocol: ProtocolId,
        conversation_id: String,
        file_id: i32,
    },
    /// Ask Telegram for this voice file. The open message must be that file
    /// and must not already have a path.
    PlayAudio {
        protocol: ProtocolId,
        conversation_id: String,
        message_id: String,
        file_id: i32,
    },
    /// Download this video for the in-app player (ADR 0015). The open
    /// message must be that file and must not already have a path.
    PlayVideo {
        protocol: ProtocolId,
        conversation_id: String,
        message_id: String,
        file_id: i32,
    },
    /// Download one document of the open message. The core sends one request
    /// until that file finishes or fails.
    DownloadDocument {
        protocol: ProtocolId,
        conversation_id: String,
        message_id: String,
        file_id: i32,
    },
    /// The next send of the open chat replies to this message. Only a
    /// protocol with `MessageActions::reply`.
    ReplyTo {
        protocol: ProtocolId,
        conversation_id: String,
        message_id: String,
    },
    /// The next send is a plain message again.
    CancelReply,
    /// Scroll the open chat to this message: the message that a clicked
    /// reply quote answers. Older pages load when it is not loaded yet.
    JumpToMessage {
        protocol: ProtocolId,
        conversation_id: String,
        message_id: String,
    },
    /// Delete one message of the open chat. `for_everyone` removes an own
    /// message for every member, where the protocol allows it.
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
    /// Edit one own text message of the open thread. Its text goes in the
    /// compose field. Only a protocol with `MessageActions::edit`.
    EditMessage {
        protocol: ProtocolId,
        conversation_id: String,
        message_id: String,
    },
    /// End the edit with no change. The draft of the thread comes back.
    CancelEdit,
    /// Add or remove one message of the open thread in the selection to
    /// forward. Only a protocol with `MessageActions::forward`.
    SelectMessage {
        protocol: ProtocolId,
        conversation_id: String,
        message_id: String,
    },
    /// End the selection mode.
    ClearSelection,
    /// Open the forward picker for one message, or for the selection of the
    /// open thread when `message_id` is `None`.
    OpenForward {
        protocol: ProtocolId,
        conversation_id: String,
        message_id: Option<String>,
    },
    /// The search text of the forward picker. `SecretText` keeps the typed
    /// text out of `Debug`.
    SetForwardQuery(SecretText),
    /// Forward to this chat of the same account. The picker and the
    /// selection close.
    ForwardTo { conversation_id: String },
    /// Close the forward picker. The selection stays.
    CancelForward,
    /// Telegram login and account actions.
    Telegram(TelegramIntent),
    /// Experimental WhatsApp pairing. Only a `whatsapp-web` build acts on it.
    /// The client runs in the WhatsApp helper process (ADR 0013).
    WhatsApp(WhatsAppIntent),
    /// Discord bot inbox. Only a `discord-bot` build acts on it.
    Discord(DiscordIntent),
    /// Slack workspace app. Only a `slack-oauth` build acts on it.
    Slack(SlackIntent),
    /// Signal linking. Only a `signal-local` build acts on it.
    Signal(SignalIntent),
}

/// A Telegram login form field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AuthField {
    ApiId,
    ApiHash,
    Phone,
    Code,
    Password,
}

/// Telegram account actions. Field values go to the memory vault only.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TelegramIntent {
    /// Start Add account. Opens the phone step or the missing-credentials step.
    AddAccount,
    /// Open the Advanced API credential override.
    OpenApiOverride,
    /// New text in one login field. The view shows it back to the frontend.
    SetField(AuthField, SecretText),
    /// Submit the current login step.
    Submit,
    /// Leave the login flow and clear every typed field.
    Cancel,
    /// Go back from the code step to the phone step.
    ChangeNumber,
    /// Ask Telegram for a new login code.
    ResendCode,
}

/// Experimental WhatsApp actions. The risk gate comes before any pairing.
///
/// The core checks the order: `AcknowledgeRisk` only while the gate is on
/// screen, and `BeginLink` only after that. Any other order is dropped. A
/// future gated protocol must follow the same rule in the core, not in a
/// frontend.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum WhatsAppIntent {
    /// Show the full-screen ToS and ban risk gate.
    OpenRiskGate,
    /// Leave the gate or the pair screen. On the pair screen this is
    /// `CancelLink`: pairing stops and the acknowledgement resets.
    CloseGate,
    /// The user accepted the risk gate.
    AcknowledgeRisk,
    /// New text in the optional phone field of the pair screen.
    SetPhone(SecretText),
    /// Start linked-device pairing. The phone goes to the memory vault only.
    BeginLink,
    /// Leave the QR and open the phone pair-code field. The risk gate stays accepted.
    LinkWithPhone,
    /// Stop pairing and drop the risk acknowledgement.
    CancelLink,
}

/// Signal linking. The notice comes before any link.
///
/// The core checks the order: `AcknowledgeNotice` only while the notice is
/// on screen, and `BeginLink` only after that. Any other order is dropped.
/// A frontend cannot skip the gate.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SignalIntent {
    /// Show the full-screen Signal notice.
    OpenNotice,
    /// Leave the notice or the link screen. On the link screen this is
    /// `CancelLink`: linking stops and the acknowledgement resets.
    CloseGate,
    /// The user accepted the Signal notice.
    AcknowledgeNotice,
    /// Start secondary-device linking.
    BeginLink,
    /// Stop linking and drop the notice acknowledgement.
    CancelLink,
    /// The phone removed this device. Open the normal QR link flow again.
    Relink,
}

/// Discord bot inbox actions. No user-account path exists (ADR 0009).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DiscordIntent {
    /// Arm the bot client with the token that is in the keychain.
    Connect,
}

/// Slack workspace app actions (ADR 0008).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SlackIntent {
    /// Start the workspace install, or resume a stored bot token.
    Connect,
    /// Stop the install. The loopback listener closes.
    Cancel,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_of_secret_intents_is_redacted() {
        let intents = [
            Intent::Telegram(TelegramIntent::SetField(
                AuthField::Code,
                SecretText::new("24680"),
            )),
            Intent::Telegram(TelegramIntent::SetField(
                AuthField::Password,
                SecretText::new("hunter2-fixture"),
            )),
            Intent::WhatsApp(WhatsAppIntent::SetPhone(SecretText::new("+15550100"))),
            Intent::SetDraft {
                protocol: ProtocolId::Telegram,
                conversation_id: "telegram:1".into(),
                topic: None,
                text: SecretText::new("draft-fixture-3b7"),
            },
            Intent::SetSearch(SecretText::new("search-fixture-6e5")),
        ];
        for intent in intents {
            let shown = format!("{intent:?}");
            for secret in [
                "24680",
                "hunter2-fixture",
                "5550100",
                "draft-fixture-3b7",
                "search-fixture-6e5",
            ] {
                assert!(!shown.contains(secret), "{shown}");
            }
        }
    }
}
