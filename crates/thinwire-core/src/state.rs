//! App state. Mutated only on the frontend thread from adapter events and user actions.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use thinwire_protocol::ProtocolKind;

#[cfg(feature = "whatsapp-web")]
use thinwire_protocol::WhatsAppPhoneVault;
use thinwire_protocol::{
    AccountState, AdapterCommand, AdapterEvent, AdapterStatus, AudioPath, ChatMessage, ChatTopic,
    Conversation, Delivery, DiscordAdapter, HelperFault, HelperState, HistoryStoreCursor,
    MAX_ATTACHMENTS, MessageAudio, MessageTopic, OutgoingFile, ProtocolCapabilities, ProtocolId,
    StoreAccessReason, TelegramApiSource, TelegramAuthError, TelegramAuthPhase, TelegramAuthStep,
    TelegramCodeVia, TelegramSecretVault, VoiceStop, catalog, ordered_topics,
    telegram_api_available, topic_mute_id, with_outbox_read,
};

use crate::mutes::{ChatMute, ChatMutes};
use crate::secrets::{SecretKey, SecretStore};
use crate::sends::{Pending, SendTracker};

/// Shown until TDLib reports Ready. Feature-off builds stay on this copy.
pub const TDLIB_UNAVAILABLE_BANNER: &str = "TDLib unavailable in this build. Enable feature telegram-tdlib after a local TDLib install. These screens do not open a live Telegram session.";

/// Shown when TDLib is compiled but authorizationStateReady has not arrived.
/// It drops only on Ready (ADR 0006). End-user copy: no library names.
pub const TELEGRAM_STUB_UNTIL_READY: &str = "Telegram is not signed in yet.";

#[must_use]
pub const fn tdlib_compiled() -> bool {
    cfg!(feature = "telegram-tdlib")
}

#[must_use]
pub fn stub_banner(snapshot: &Snapshot) -> Option<&'static str> {
    if snapshot.telegram_ready() {
        return None;
    }
    if tdlib_compiled() {
        Some(TELEGRAM_STUB_UNTIL_READY)
    } else {
        Some(TDLIB_UNAVAILABLE_BANNER)
    }
}

/// README risk text for an unofficial account. `None` for an official path.
/// The review window shows these lines. Signal omits the README sentence
/// that only describes the screen.
#[must_use]
pub fn unofficial_notice(protocol: ProtocolId) -> Option<&'static [&'static str]> {
    match protocol.kind() {
        ProtocolKind::WhatsApp => Some(thinwire_protocol::CRITIC_RISK_BULLETS.as_slice()),
        ProtocolKind::Signal => Some(crate::signal_notice::SIGNAL_NOTICE_REVIEW.as_slice()),
        ProtocolKind::Telegram | ProtocolKind::Discord | ProtocolKind::Slack => None,
    }
}

/// Label of the account-info button and of the review window.
/// WhatsApp is "Ban risk". Signal is "Risk".
#[must_use]
pub const fn notice_label(protocol: ProtocolId) -> Option<&'static str> {
    match protocol.kind() {
        ProtocolKind::WhatsApp => Some("Ban risk"),
        ProtocolKind::Signal => Some("Risk"),
        ProtocolKind::Telegram | ProtocolKind::Discord | ProtocolKind::Slack => None,
    }
}

/// Compile-time chrome gate: spikes stay invisible when their feature is off.
#[must_use]
pub const fn protocol_chrome_enabled(protocol: ProtocolId) -> bool {
    match protocol.kind() {
        ProtocolKind::Telegram => true,
        ProtocolKind::Slack => cfg!(feature = "slack-oauth"),
        ProtocolKind::WhatsApp => cfg!(feature = "whatsapp-web"),
        ProtocolKind::Discord => cfg!(feature = "discord-bot"),
        ProtocolKind::Signal => cfg!(feature = "signal-local"),
    }
}

/// Non-modal Telegram login steps. Credential field values never leave this snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthScreen {
    Idle,
    NeedCredentials,
    TelegramApi,
    /// The client is starting. The phone step shows only after the adapter
    /// reports `NeedPhone` (TDLib `authorizationStateWaitPhoneNumber`).
    TelegramConnecting,
    TelegramPhone,
    TelegramCode,
    Telegram2fa,
}

/// Start-up resume of a saved Telegram session. Runs once per launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resume {
    /// The OS keychain has not finished its first read.
    Waiting,
    /// A saved session exists. TDLib is starting with no click.
    Connecting,
    /// Resume finished, failed, or did not apply.
    Settled,
}

/// What the center panel shows. Pure state, so tests do not need egui.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CenterView {
    Auth,
    /// Spinner. `true` adds the "Connecting to Telegram…" text.
    Resuming {
        connecting: bool,
    },
    FirstRun,
    /// The keychain opened, but a read failed. Try again reads it again.
    KeychainFailed,
    Thread,
}

/// What the inbox list shows for the selected protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboxState {
    Rows,
    Loading,
    Empty,
    /// Chats exist, but the search hides all of them.
    NoMatch,
}

/// Older-message paging of the selected chat (#30).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OlderState {
    /// More can load when the thread reaches the top.
    Idle,
    /// "Loading older messages…"
    Loading,
    /// "Start of chat": nothing older.
    StartOfChat,
}

/// One history page that records unreadable rows.
struct UnreadablePage<'a> {
    protocol: ProtocolId,
    chat: &'a str,
    topic: Option<MessageTopic>,
    request_before: &'a str,
    cursor: HistoryStoreCursor,
    more: bool,
    initial: bool,
}

/// One closed run of stored rows the thread could not read.
///
/// The line is drawn once. `before_id` is the message the run sits older
/// than. `None` means the run is the newest content, so it is drawn after
/// every message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnreadableRun {
    pub before_id: Option<String>,
    /// `0` means the count is not known.
    pub skipped: u32,
}

/// The weak line for one unreadable run. `skipped == 0` is the sentence
/// used when the count is not known.
#[must_use]
pub fn unreadable_messages_line(skipped: u32) -> String {
    match skipped {
        0 => "Some messages here could not be read. They were not deleted.".to_string(),
        1 => "1 message here could not be read. It was not deleted.".to_string(),
        count => format!("{count} messages here could not be read. They were not deleted."),
    }
}

fn push_unreadable(runs: &mut Vec<UnreadableRun>, before_id: Option<String>, skipped: u32) {
    if skipped == 0 {
        return;
    }
    if let Some(run) = runs.iter_mut().find(|run| run.before_id == before_id) {
        run.skipped = run.skipped.saturating_add(skipped);
        return;
    }
    runs.push(UnreadableRun { before_id, skipped });
}

/// Skips not drawn yet. Empty pages add to it until a page shows rows or
/// the store ends.
struct OpenUnreadable {
    before_id: Option<String>,
    skipped: u32,
}

/// What the thread shows for the selected chat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadState {
    NoSelection,
    Rows,
    Loading,
    Empty,
}

/// Keys the login form reacts to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthKey {
    Enter,
    Escape,
}

/// Error block for a refused login step. Only the error kind is shown.
#[must_use]
pub fn auth_user_error(reason: Option<TelegramAuthError>) -> UserError {
    let (happened, why, next) = match reason {
        Some(TelegramAuthError::PhoneInvalid) => (
            "Telegram did not accept the phone number.",
            "The number format is not valid.".to_string(),
            "Check the number. Use + and the country code.".to_string(),
        ),
        Some(TelegramAuthError::CodeInvalid) => (
            "Telegram did not accept the code.",
            "The code is wrong.".to_string(),
            "Type it again.".to_string(),
        ),
        Some(TelegramAuthError::CodeExpired) => (
            "Telegram did not accept the code.",
            "The code expired.".to_string(),
            "Press Send a new code.".to_string(),
        ),
        Some(TelegramAuthError::PasswordInvalid) => (
            "Telegram did not accept the password.",
            "The password is wrong.".to_string(),
            "Type it again.".to_string(),
        ),
        Some(TelegramAuthError::FloodWait { seconds }) => {
            let minutes = seconds.div_ceil(60).max(1);
            let unit = if minutes == 1 { "minute" } else { "minutes" };
            (
                "Telegram paused the login.",
                "Too many tries.".to_string(),
                format!("Wait {minutes} {unit}, then try again."),
            )
        }
        Some(TelegramAuthError::ClientSetup { code }) => (
            "Telegram could not start.",
            format!("The local Telegram data could not be opened (error {code})."),
            "Press Add Telegram to try again. If it fails again, restart thinwire.".to_string(),
        ),
        Some(TelegramAuthError::Other { code }) => (
            "Telegram login did not advance.",
            format!("Telegram did not accept this step (error {code})."),
            "Correct the field, or press Cancel.".to_string(),
        ),
        None => (
            "Telegram login did not advance.",
            "Telegram did not accept this step.".to_string(),
            "Correct the field, or press Cancel.".to_string(),
        ),
    };
    UserError {
        happened: happened.into(),
        why,
        next,
    }
}

/// Copy on the phone step after the old Telegram data folder was moved aside.
/// Names the folder once; old folders are kept, never deleted.
#[must_use]
/// The keychain lost the session marker, but TDLib can still hold the
/// session: the TDLib folder has a database and the keychain has its key.
fn unmarked_database(store: &SecretStore) -> bool {
    store.tdlib_database_found() && store.get(SecretKey::DbEncryption).ok().flatten().is_some()
}

pub fn data_reset_notice(moved_to: &str) -> String {
    format!(
        "Telegram data on this device could not be opened. It was moved to \"{moved_to}\" in the thinwire data folder and kept. Sign in again."
    )
}

/// Status line when Add Telegram is pressed before the keychain read ends.
const KEYCHAIN_LOADING_STATUS: &str = "Reading the keychain. Try again in a moment.";

/// Copy on the phone step when a saved session no longer works.
pub const SESSION_ENDED_NOTICE: &str = "Your Telegram session ended. Sign in again.";

/// Status line while a new client starts, before the phone step.
const CONNECTING_STATUS: &str = "Connecting to Telegram…";

/// Center panel copy when a keychain read failed (the values are unknown).
pub const KEYCHAIN_READ_FAILED: &str = "The keychain could not be read. Your saved sign-in did not load. Unlock the keychain, then press Try again.";

/// Status lines of a running load (#80). The strip shows them before any
/// finished line.
pub const LOADING_CHATS_STATUS: &str = "Loading chats…";
/// Middle column after a link, when that account has no chats.
pub const NO_CHATS_YET: &str = "No chats yet.";
pub const LOADING_MESSAGES_STATUS: &str = "Loading messages…";
pub const LOADING_OLDER_STATUS: &str = "Loading older messages…";

/// The thread's note after a failed older page. The adapter's detail (for
/// example a library name and an error code) goes to the debug log only.
pub const OLDER_PAGE_FAILED_NOTE: &str = "Could not load older messages. Try again later.";
pub const SENDING_STATUS: &str = "Sending…";

/// Center panel copy while the keychain read runs.
pub const KEYCHAIN_OPENING: &str = "Opening the keychain…";

/// Center panel copy when the keychain read takes long: a wallet can wait
/// for an unlock prompt, which may be behind this window.
pub const KEYCHAIN_WAITING: &str = "Waiting for the keychain. Unlock it to continue.";

/// After this long, the keychain copy asks the user to unlock it.
const KEYCHAIN_SLOW_AFTER: Duration = Duration::from_secs(1);

/// Shortest wait before the same older-message anchor is asked again, after
/// a request that brought nothing older (a failure or an anchor-only page).
/// The frontend also asks only when the view enters the top again (#61).
pub const OLDER_RETRY_DELAY: Duration = Duration::from_secs(2);

/// Longest wait between tries of one anchor. Each try that brings nothing
/// older doubles the wait, up to this (#67).
pub const OLDER_RETRY_MAX: Duration = Duration::from_secs(60);

/// Wait before try `tries + 1` of an anchor that brought nothing `tries` times.
fn older_wait(tries: u32) -> Duration {
    OLDER_RETRY_DELAY
        .saturating_mul(1 << tries.saturating_sub(1).min(16))
        .min(OLDER_RETRY_MAX)
}

/// Center panel copy while a saved session reconnects.
pub const RESUME_CONNECTING: &str = "Connecting to Telegram…";

/// Status line before any sign-in.
const SIGN_IN_PROMPT: &str = "Sign in with Telegram to get started.";

/// Longest text of the phone field that a pairing start takes. A phone
/// number has 15 digits at most (E.164). This bound leaves room for "+",
/// spaces, and separators.
#[cfg(feature = "whatsapp-web")]
pub const WHATSAPP_PHONE_MAX_CHARS: usize = 32;

/// The note under the phone field when its text is too long.
#[cfg(feature = "whatsapp-web")]
pub const WHATSAPP_PHONE_TOO_LONG: &str =
    "This phone number is too long. Type the country code and the number, digits only.";

/// Experimental WhatsApp screens. Only the `whatsapp-web` build can enter them.
#[cfg(feature = "whatsapp-web")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhatsAppScreen {
    Hidden,
    RiskGate,
    Pair,
}

/// A send or retry that expired and shows in the timeout error (#69). A late
/// accept removes only its own entry (PR #81 review).
#[derive(Debug, Clone, PartialEq, Eq)]
struct TimedOut {
    protocol: ProtocolId,
    chat: String,
    request: u64,
    retry: bool,
    topic: Option<MessageTopic>,
}

/// Signal screens. Only the `signal-local` build can enter them.
#[cfg(feature = "signal-local")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalScreen {
    Hidden,
    Notice,
    Link,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserError {
    pub happened: String,
    pub why: String,
    pub next: String,
}

/// Whether a previous Signal store could be read.
/// The core sets this. The row text is not this state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetainedRead {
    /// The store can be read, or this row is the live account.
    Readable,
    /// This publish could not read the store. The row stays and is disabled.
    Unreadable,
}

#[derive(Debug, Clone)]
pub struct AccountRow {
    pub caps: ProtocolCapabilities,
    pub status: AdapterStatus,
    pub detail: String,
    /// Link state. `AdapterEvent::Account` changes it. A wrong key also
    /// leaves Linked. A `Status` never does.
    pub state: AccountState,
    /// State of the protocol's helper process (ADR 0013). `None` for a
    /// protocol that runs in the app, and before its helper reports.
    pub helper: Option<HelperState>,
    /// A previous Signal account kept in `retained/`. `None` is the live
    /// account of every protocol, including the linked Signal number.
    /// The number is a slot, not an account id.
    pub signal_retained: Option<u32>,
    /// The session key could not be used. `None` after the next status.
    /// A denied read shows Retry. A failed move shows Retry and stays
    /// linked. A wrong key shows Link and leaves Linked.
    pub store_access: Option<StoreAccessReason>,
    /// Read state of a previous Signal store.
    /// The core sets [`RetainedRead::Unreadable`] when it handles the
    /// unreadable line. A later chat upsert clears it.
    pub retained_read: RetainedRead,
}

impl AccountRow {
    /// Signed in: the shell shows this protocol's chats.
    #[must_use]
    pub fn linked(&self) -> bool {
        self.state == AccountState::Linked
    }

    /// This previous-account store could not be read.
    /// The row stays in the list. It is disabled. It has no composer.
    #[must_use]
    pub const fn history_unreadable(&self) -> bool {
        self.signal_retained.is_some() && matches!(self.retained_read, RetainedRead::Unreadable)
    }

    /// The helper stopped and does not start again by itself. The row
    /// shows "Helper stopped." and Restart.
    #[must_use]
    pub const fn helper_stopped(&self) -> bool {
        matches!(self.helper, Some(HelperState::Stopped(_)))
    }

    /// The helper program is not installed. The protocol stays visible, and
    /// its entry in Add account is disabled.
    #[must_use]
    pub const fn helper_missing(&self) -> bool {
        matches!(self.helper, Some(HelperState::Missing))
    }

    /// The line of the account row for the helper state. `None`: the row
    /// shows its normal state.
    #[must_use]
    pub const fn helper_label(&self) -> Option<&'static str> {
        match self.helper {
            Some(HelperState::Busy) => Some(HELPER_BUSY),
            Some(HelperState::Restarting) => Some(HELPER_RESTARTING),
            Some(HelperState::Stopped(_)) => Some(HELPER_STOPPED),
            Some(HelperState::Missing) => Some(HELPER_MISSING),
            Some(HelperState::Idle | HelperState::Running) | None => None,
        }
    }

    /// "<Protocol> helper missing. Reinstall thinwire." (ADR 0013).
    #[must_use]
    pub fn helper_missing_text(&self) -> String {
        format!(
            "{} helper missing. Reinstall thinwire.",
            self.caps.id.display_name()
        )
    }
}

/// What a full-screen gate shows about the helper of its protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HelperNotice<'a> {
    /// The line of the account row, for example "Helper stopped."
    pub label: &'static str,
    /// What happened, why, and what to do, when the helper stopped.
    pub error: Option<&'a UserError>,
}

/// The tray note when a dropped or pasted file is not a photo or a video.
pub const ATTACH_ONLY_MEDIA: &str = "Only photos and videos can be attached.";
/// The tray note when the tray is full.
pub const ATTACH_TOO_MANY: &str = "30 files at most in one send.";
/// The tray note when this account cannot send files.
pub const ATTACH_NOT_HERE: &str = "This account cannot send photos or videos.";

/// Identity of the WhatsApp ban gate copy: its README risk bullets. The
/// settings file keeps it when the user accepts the gate. A saved value of
/// another copy does not count, so a new copy shows the gate again.
pub const WHATSAPP_GATE_VERSION: u64 =
    crate::signal_notice::notice_version(&thinwire_protocol::CRITIC_RISK_BULLETS);

/// Row line of an account after account 0 whose saved session ended.
pub const ACCOUNT_SIGN_IN_AGAIN: &str = "Signed out. Click Link to sign in again.";

/// Status line after the user starts a WhatsApp pairing.
pub const WHATSAPP_PAIRING_REQUESTED: &str = "WhatsApp pairing requested.";
/// Status line after the user accepts the WhatsApp ban gate.
pub const WHATSAPP_GATE_ACCEPTED: &str = "WhatsApp ban gate accepted. Pairing has not started.";
/// Status line after WhatsApp links. It replaces the pairing lines.
pub const WHATSAPP_LINKED: &str = "WhatsApp is linked.";

/// Account row line while the helper starts or does not answer in time.
pub const HELPER_BUSY: &str = "Helper is busy…";
/// Account row line while the core waits to start the helper again.
pub const HELPER_RESTARTING: &str = "Helper stopped. Restarting…";
/// Account row line after the last restart failed. Restart is next to it.
pub const HELPER_STOPPED: &str = "Helper stopped.";
/// Account row line when the helper program is not installed.
pub const HELPER_MISSING: &str = "Helper missing";

/// The sentence under the chip for a session-key problem.
///
/// A saved account (`linked`) that cannot read the key gets the retry
/// sentence. A first link gets the "did not link" sentence.
fn store_access_detail(
    protocol: ProtocolId,
    reason: StoreAccessReason,
    linked: bool,
) -> &'static str {
    match reason {
        StoreAccessReason::KeychainDenied if linked => thinwire_protocol::KEYCHAIN_ACCESS_DENIED,
        StoreAccessReason::KeychainDenied if protocol.is(ProtocolKind::WhatsApp) => {
            thinwire_protocol::whatsapp_keychain_unavailable()
        }
        StoreAccessReason::KeychainDenied => thinwire_protocol::signal_keychain_unavailable(),
        StoreAccessReason::WrongKey if protocol.is(ProtocolKind::WhatsApp) => {
            thinwire_protocol::whatsapp_session_kept()
        }
        StoreAccessReason::WrongKey => thinwire_protocol::signal_session_kept(),
        StoreAccessReason::MoveFailed if protocol.is(ProtocolKind::WhatsApp) => {
            thinwire_protocol::whatsapp_key_wrong_kept()
        }
        StoreAccessReason::MoveFailed => thinwire_protocol::signal_key_wrong_kept(),
    }
}

/// A status line about the retained Signal history, not about the live
/// account.
fn is_signal_retained_line(detail: &str) -> bool {
    [
        thinwire_protocol::SIGNAL_PREVIOUS_ACCOUNT,
        thinwire_protocol::SIGNAL_RETAINED_RESET,
        thinwire_protocol::SIGNAL_RETAINED_SETTLED,
    ]
    .contains(&detail)
        || thinwire_protocol::signal_retained_unreadable_slot(detail).is_some()
}

/// The move-aside failed. The files stayed, so a saved account stays linked.
fn stores_left_in_place(detail: &str) -> bool {
    detail == thinwire_protocol::signal_key_wrong_kept()
        || detail == thinwire_protocol::whatsapp_key_wrong_kept()
}

/// Drop a trailing "did not link" sentence. A saved account does not use it.
fn without_did_not_link(detail: &str) -> String {
    detail
        .replace(" Signal did not link.", "")
        .replace(" WhatsApp did not link.", "")
}

/// Shown in the open chat when the phone removed this Signal device.
/// Sending does not go out. The history stays.
pub const SIGNAL_SENDING_OFF: &str = "Sending is off.";

/// Why `sync_focused_row` runs. A search change or a key move scrolls.
/// A chat update does not: the list stays where the user left it.
enum FocusFollow {
    Query,
    List,
    Moved,
    /// A row was inserted, updated, or reordered. Keep the highlight id.
    Upsert,
}

/// Text the adapter rejected for one chat. A compose send and a retry are
/// separate: the compose text wins when it still matches, otherwise the
/// retry text fills an empty draft.
#[derive(Default)]
struct RejectedBody {
    compose: Option<String>,
    row: Option<String>,
    /// The topic the rejected text belongs to. `None` is a chat with no topic.
    topic: Option<MessageTopic>,
}

/// App state. `Debug` is manual: it prints no secret and no user text.
/// One thread: protocol, chat, and topic. `None` is a chat with no topic.
type OlderPageKey = (ProtocolId, String, Option<MessageTopic>);

/// An older page that brought nothing new: the anchor, when, and how many times.
type OlderRetry = (String, Instant, u32);

/// The side of a pane in the split view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitSide {
    Left,
    Right,
}

impl SplitSide {
    const fn other(self) -> Self {
        match self {
            Self::Left => Self::Right,
            Self::Right => Self::Left,
        }
    }
}

/// What one pane of the split view shows. `Debug` prints no chat id and no
/// search text.
#[derive(Clone, PartialEq, Eq)]
struct PaneView {
    protocol: ProtocolId,
    signal_retained: Option<u32>,
    conversation: Option<String>,
    topic: Option<MessageTopic>,
    search: String,
}

impl std::fmt::Debug for PaneView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PaneView")
            .field("protocol", &self.protocol)
            .field("has_chat", &self.conversation.is_some())
            .finish()
    }
}

/// The split view. The `selected_*` fields of the snapshot are the pane
/// with the focus, and `other` is the second pane.
#[derive(Debug)]
struct Split {
    other: PaneView,
    /// The side of the pane with the focus.
    active: SplitSide,
    /// The compose text and the keyboard highlight of the pane with the
    /// focus, while the frontend draws the other pane (`peek_other_pane`).
    peeked: Option<(String, Option<String>)>,
}

pub struct Snapshot {
    pub accounts: Vec<AccountRow>,
    conversations: HashMap<ProtocolId, Vec<Conversation>>,
    messages: HashMap<(ProtocolId, String, Option<MessageTopic>), Vec<ChatMessage>>,
    /// Forum topics and bot threads, keyed by protocol and chat.
    topics: HashMap<(ProtocolId, String), Vec<ChatTopic>>,
    pub selected_protocol: ProtocolId,
    pub selected_conversation: Option<String>,
    /// The open topic of the selected chat. `None` is the topic list, or a
    /// chat that has no topics.
    pub selected_topic: Option<MessageTopic>,
    /// Keyboard highlight. Arrows move this. Enter and a click open the chat.
    pub focused_row: Option<String>,
    /// Read-only notice opened from Unofficial or Ban risk. It does not
    /// start a link and it does not cancel one.
    notice_review: Option<ProtocolId>,
    pub search: String,
    pub auth: AuthScreen,
    pub telegram_api_id: String,
    pub telegram_api_hash: String,
    pub telegram_phone: String,
    pub telegram_code: String,
    pub telegram_2fa: String,
    pub error: Option<UserError>,
    pub status_text: String,
    /// The last Telegram status line that came as `AdapterStatus::Ready`: a
    /// finished success such as "Message sent." (#56).
    ready_status: Option<String>,
    /// The last Telegram status line that came as `Error` or `Refused`. While
    /// `status_text` still holds it, it wins over a load of another protocol.
    /// The selected protocol's own load still shows (#90, #73).
    failure_status: Option<String>,
    /// Accepted Telegram sends still waiting for their own end.
    /// Each entry is that request, the chat, the time of `SendAccepted`, and
    /// the pending message id once Telegram has one. `SendAccepted` drops the
    /// request from [`Self::sends`], but the strip stays on Sending… until
    /// [`Self::end_telegram_send`] releases that request. A status line does
    /// not end it.
    telegram_send_unconfirmed: Vec<TelegramHold>,
    /// Outbound pending message ids seen while a Telegram send is in flight,
    /// before `SendAccepted` binds them to that request. Keyed by request, so
    /// a late accept reclaims its own id and a newer send does not. One id for
    /// each request, kept only while the send tracker keeps that request.
    telegram_unbound_messages: Vec<TelegramQueuedId>,
    /// Last outgoing message id the other party has read, per chat.
    /// Telegram `last_read_outbox_message_id`. A later id does not move it back.
    outbox_read: HashMap<(ProtocolId, String), i64>,
    pub compose: String,
    pub auth_busy: bool,
    /// One line above the active login form. Never holds a secret.
    pub auth_notice: Option<String>,
    /// Why Telegram refused the last login step, if it said.
    pub auth_rejection: Option<TelegramAuthError>,
    /// Where Telegram sent the login code, if it said.
    pub code_via: Option<TelegramCodeVia>,
    pub telegram_authorized: bool,
    resume: Resume,
    /// The resume has no session marker: only a TDLib database and its key.
    /// A login step from TDLib then closes the client with no screen.
    resume_unmarked: bool,
    /// When the UI first saw the keychain read still running.
    keychain_wait_started: Option<Instant>,
    /// The last keychain read failed. Copied from the store each frame.
    keychain_failed: bool,
    /// The user asked to read the keychain again. The app runs it off the UI thread.
    keychain_retry: bool,
    /// A saved account could not read its session key while the app
    /// keychain was not settled yet. One later pump sends `RetryStoreKey`
    /// when that read succeeds. A denial after the keychain is ready does
    /// not set this. The user clicks Retry for that one.
    store_key_retry_due: bool,
    /// The app keychain could be read at the start of this pump.
    app_keychain_ready: bool,
    /// Protocols whose chat list is loading.
    chat_list_loading: HashSet<ProtocolId>,
    /// Chats whose history is loading, per protocol.
    history_loading: HashSet<(ProtocolId, String)>,
    /// The last note of each protocol (`AdapterEvent::Notice`). Never an error.
    notices: HashMap<ProtocolId, String>,
    /// The chat the adapters were last told the user looks at (`ViewChat`
    /// or `ViewTopic`). The topic is `None` for a chat that has no topics.
    /// The topic list is not a viewed chat.
    viewed: Option<(ProtocolId, String, Option<MessageTopic>)>,
    /// The window has focus. An open chat stays unread until it does, so a
    /// notification while the user is away still has a badge when they return.
    window_focused: bool,
    /// The expired sends that the timeout error lists, and the error as the
    /// core last set it. When `error` is no longer that value (the user
    /// closed it, or another error came), the list starts again.
    timed_out: Vec<TimedOut>,
    timeout_error: Option<UserError>,

    /// A chat the user opened while its protocol was not linked. Its history
    /// did not load, so `Linked` loads it (#70). A normal reconnect with no
    /// such chat sends no second `OpenChat`.
    open_on_link: Option<(ProtocolId, String)>,
    /// Protocols with a session: they reached `Linked` and did not end. A
    /// `Linking` after `Linked` is a reconnect; the session stays (ADR 0010).
    sessions: HashSet<ProtocolId>,
    /// Protocols the shell shows even with their feature off: the demo
    /// adapters (#120), and tests of the shell with more than Telegram.
    pub(crate) extra_visible: HashSet<ProtocolId>,
    /// Test hook: protocols the shell hides even with their feature on, so a
    /// test holds in every feature build (qa on #111).
    #[cfg(any(test, feature = "test-support"))]
    hidden_for_test: HashSet<ProtocolId>,
    /// Threads with a request for older messages in flight (#30), keyed by
    /// protocol, chat, and topic. `None` is a chat with no topic. One request
    /// at a time for each thread.
    older_loading: HashSet<OlderPageKey>,
    /// Protocols whose chat-list load a reconnect ended. `Linked` loads the
    /// list again (#163).
    list_on_link: HashSet<ProtocolId>,
    /// Threads whose start is loaded. No more older requests (#30).
    older_at_start: HashSet<OlderPageKey>,
    /// Threads whose last older request brought nothing older: the anchor it
    /// used and when it ended. That anchor waits `OLDER_RETRY_DELAY` (#61
    /// review).
    older_retry: HashMap<OlderPageKey, OlderRetry>,
    /// Threads whose last older page did not load. A later success clears
    /// that thread. The thread shows plain text only (#57).
    older_note: HashSet<OlderPageKey>,
    /// Closed runs of unreadable stored rows, oldest request first.
    unreadable: HashMap<OlderPageKey, Vec<UnreadableRun>>,
    /// A run that is still loading. Empty pages add to it.
    open_unreadable: HashMap<OlderPageKey, OpenUnreadable>,
    /// The next older read of this thread starts at this store timestamp.
    history_resume: HashMap<OlderPageKey, u64>,
    /// The store timestamp last sent for this thread. The next cursor must
    /// be older, so a stuck cursor cannot loop.
    history_asked: HashMap<OlderPageKey, u64>,
    /// Scroll the open chat into view once, after the user opens it.
    /// A message that reorders the list does not set this.
    scroll_to_selected: bool,
    scroll_to_focused: bool,
    /// The thread scrolls this message into view once: the message that a
    /// clicked reply quote answers.
    scroll_to_message: Option<String>,
    /// A clicked reply quote whose message is not loaded yet. Older pages
    /// load until the message arrives or the page budget ends.
    reply_jump: Option<ReplyJump>,
    /// Inbox row ids last seen by `sync_focused_row`. A list change is a difference here.
    seen_visible_ids: Vec<String>,
    /// Unsent compose text per chat and topic. `compose` holds the open
    /// thread's draft. A chat with no topics uses `None`.
    drafts: HashMap<(ProtocolId, String, Option<MessageTopic>), String>,
    /// The message that the next send of its thread replies to. Another
    /// open thread does not show it or use it.
    replying: Option<ReplyTarget>,
    /// The own message that the compose field edits. Enter saves the edit,
    /// and Esc puts the draft of the thread back.
    editing: Option<EditTarget>,
    /// The messages of one thread that the user selected to forward.
    selection: Option<MessageSelection>,
    /// The open forward picker: the messages it forwards and its search.
    forwarding: Option<ForwardDraft>,
    /// The typing state of this account, and who types in each chat (#510).
    typing: crate::typing::Typing,
    /// Photos and videos attached to a thread and not sent yet, in order.
    attachments: HashMap<(ProtocolId, String, Option<MessageTopic>), Vec<OutgoingFile>>,
    /// Why the last attach skipped files. The tray shows it until the next
    /// change of the tray.
    attachment_note: Option<&'static str>,
    /// Body of a send or retry the adapter rejected, until that protocol's
    /// session ends. An unlink keeps one only when nothing newer replaced it.
    /// A Telegram session end drops them.
    rejected_bodies: HashMap<(ProtocolId, String), RejectedBody>,
    focus_compose: bool,
    /// Protocols that answered `Shutdown` with `Stopped`.
    stopped: HashSet<ProtocolId>,
    /// Sends and retries in flight, one per (protocol, chat). The text stays
    /// until the adapter accepts it; only the request's own `SendAccepted` /
    /// `SendRejected` ends an entry (PR #40 review, shell plan items 4, 10).
    sends: SendTracker,
    /// The shell's id for the next pairing (WhatsApp or Signal).
    #[cfg(any(feature = "whatsapp-web", feature = "signal-local"))]
    next_pairing_generation: u64,
    /// The id of the pairing that runs now, for each protocol. A QR or pair
    /// code of any other pairing is dropped (shell plan 12). No entry: no
    /// pairing of that protocol runs. One entry for each protocol: the end
    /// of a WhatsApp pairing must not end a Signal link, and the reverse.
    #[cfg(any(feature = "whatsapp-web", feature = "signal-local"))]
    pairing_generation: HashMap<ProtocolId, u64>,
    /// Name of the folder the worker moved aside. Shown on the next phone step.
    data_reset: Option<String>,
    api_source: TelegramApiSource,
    /// Telegram photo downloads in flight, keyed by protocol and file id.
    photo_downloads: HashSet<(ProtocolId, i32)>,
    file_downloads: HashSet<(ProtocolId, i32)>,
    pending: Vec<AdapterCommand>,
    /// The Telegram account that the login screens are for (ADR 0014).
    /// Login steps go to it, and only its login events change the screens.
    telegram_login_account: ProtocolId,
    /// The WhatsApp account that the gate and the pair screen are for.
    whatsapp_link_account: ProtocolId,
    /// The Signal account that the notice and the link screen are for.
    signal_link_account: ProtocolId,
    keychain_flush: bool,
    /// Accounts that ended for good in this pump: a Telegram session that
    /// another device or Telegram ended, or `AccountEnded` (a WhatsApp
    /// logout on the phone). The core drops their thinwire mutes (#153
    /// review). A reconnect or an app stop never adds one.
    ended_accounts: Vec<ProtocolId>,
    /// What happened, why, and what to do for a helper that stopped, by
    /// protocol. A full-screen gate shows it. The error block can hold the
    /// error of another protocol by then.
    helper_errors: HashMap<ProtocolId, UserError>,
    #[cfg(feature = "whatsapp-web")]
    pub whatsapp_screen: WhatsAppScreen,
    #[cfg(feature = "whatsapp-web")]
    pub whatsapp_phone: String,
    /// Why the last pairing start did not go out, for the phone field.
    #[cfg(feature = "whatsapp-web")]
    pub whatsapp_phone_note: Option<&'static str>,
    #[cfg(feature = "whatsapp-web")]
    pub whatsapp_qr: Option<String>,
    #[cfg(feature = "whatsapp-web")]
    pub whatsapp_pair_code: Option<String>,
    /// The helper reported that the pairing QR codes are used up. The pair
    /// screen hides the image and offers a new code. `Unlinked` follows that
    /// report, and the flag stays. A new code or a new payload clears it.
    #[cfg(feature = "whatsapp-web")]
    pub whatsapp_qr_exhausted: bool,
    #[cfg(feature = "whatsapp-web")]
    pub whatsapp_started: bool,
    /// The user accepted the full-screen ban gate in this session. Pairing
    /// needs it; showing the gate again or cancelling clears it.
    #[cfg(feature = "whatsapp-web")]
    whatsapp_risk_acknowledged: bool,
    #[cfg(feature = "signal-local")]
    pub signal_screen: SignalScreen,
    #[cfg(feature = "signal-local")]
    pub signal_qr: Option<String>,
    #[cfg(feature = "signal-local")]
    pub signal_started: bool,
    /// `Ready` has arrived for this link. Later failures are session errors.
    /// Startup and linking failures happen before this and keep the link error.
    #[cfg(feature = "signal-local")]
    signal_inbox_open: bool,
    /// The user accepted the full-screen Signal notice for the current notice
    /// copy ([`crate::signal_notice::SIGNAL_NOTICE_VERSION`]). Linking needs
    /// it. The settings file keeps that acceptance, so the next launch starts
    /// the helper without the notice. The acceptance covers the notice text.
    /// A session end clears this flag and leaves the saved hash. Cancel, and
    /// opening the notice again while the account is not linked, clear both.
    #[cfg(feature = "signal-local")]
    signal_notice_acknowledged: bool,
    /// Cancel, or an explicit re-show while the account is not linked. The
    /// settings sync drops the saved hash. A session end does not set this.
    #[cfg(feature = "signal-local")]
    signal_notice_forget: bool,
    /// The phone removed this linked device. The row and the open chat stay.
    /// Local history stays. Sending is off until the user links again.
    #[cfg(feature = "signal-local")]
    signal_removed: bool,
    /// One unauthorized receive open is waiting for a second open. The row
    /// shows the check. Keys stay, and this is not removal.
    #[cfg(feature = "signal-local")]
    signal_checking: bool,
    /// The previous Signal account open in the switcher. `None` is the live
    /// Signal number, or another protocol. The number is a slot, not an
    /// account id.
    selected_signal_retained: Option<u32>,
    /// The split view: the pane that does not have the focus. `None` is
    /// one pane.
    split: Option<Split>,
    /// Slots this retained publish has sent. A slot whose store could not
    /// be read is in the set too, so its row stays. `None` when no publish
    /// is open. A helper that has stopped, and an account end, clear it.
    /// A slot that this publish did not send is dropped when it settles.
    /// The values are counts, not account ids.
    retained_publish: Option<HashSet<u32>>,
}

/// Redacted: login fields, the WhatsApp phone and pairing material, the
/// The longest emoji a reaction may name, in bytes. A family or a flag with
/// skin tones fits.
const MAX_EMOJI_BYTES: usize = 32;

/// The messages of one thread that the user selected, in the order of
/// selection. The ids hold no message text.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MessageSelection {
    protocol: ProtocolId,
    conversation_id: String,
    topic: Option<MessageTopic>,
    message_ids: Vec<String>,
}

/// What the forward picker forwards, and the text of its chat search.
#[derive(Clone, PartialEq, Eq)]
struct ForwardDraft {
    protocol: ProtocolId,
    conversation_id: String,
    message_ids: Vec<String>,
    query: String,
}

impl std::fmt::Debug for ForwardDraft {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ForwardDraft")
            .field("protocol", &self.protocol)
            .field("conversation_id", &self.conversation_id)
            .field("message_ids", &self.message_ids)
            .field("query_len", &self.query.len())
            .finish()
    }
}

/// The forward picker as a frontend draws it.
#[derive(Debug)]
pub struct ForwardPicker<'a> {
    /// How many messages the picker forwards.
    pub count: usize,
    /// The search text.
    pub query: &'a str,
    /// The chats of the same account that match the search, in inbox order.
    pub targets: Vec<&'a Conversation>,
}

/// A message can be forwarded when the server has it: not a send that
/// waits or failed.
#[must_use]
pub const fn can_forward(message: &ChatMessage) -> bool {
    !matches!(message.delivery, Delivery::Pending | Delivery::Failed)
}

/// The own message that the compose field of one thread edits.
#[derive(Clone, PartialEq, Eq)]
struct EditTarget {
    protocol: ProtocolId,
    conversation_id: String,
    topic: Option<MessageTopic>,
    message_id: String,
    /// The compose text before the edit. It comes back after the edit.
    draft: String,
}

impl std::fmt::Debug for EditTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EditTarget")
            .field("protocol", &self.protocol)
            .field("draft_len", &self.draft.len())
            .finish_non_exhaustive()
    }
}

/// The message that the next send of one thread replies to.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ReplyTarget {
    protocol: ProtocolId,
    conversation_id: String,
    topic: Option<MessageTopic>,
    message_id: String,
}

/// Signal provisioning URL, compose, drafts, search, chat titles, and
/// message bodies never reach `Debug` (PR #48 review). Only screens, flags,
/// and counts are printed.
impl std::fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let accounts: Vec<(ProtocolId, AdapterStatus, AccountState)> = self
            .accounts
            .iter()
            .map(|row| (row.caps.id, row.status, row.state))
            .collect();
        let mut out = f.debug_struct("Snapshot");
        out.field("accounts", &accounts)
            .field("selected_protocol", &self.selected_protocol)
            .field("notice_review", &self.notice_review)
            .field("auth", &self.auth)
            .field("auth_busy", &self.auth_busy)
            .field("telegram_authorized", &self.telegram_authorized)
            .field(
                "conversations",
                &self.conversations.values().map(Vec::len).sum::<usize>(),
            )
            .field(
                "messages",
                &self.messages.values().map(Vec::len).sum::<usize>(),
            )
            .field("has_error", &self.error.is_some())
            .field("pending_commands", &self.pending.len());
        #[cfg(feature = "whatsapp-web")]
        out.field("whatsapp_screen", &self.whatsapp_screen)
            .field("whatsapp_started", &self.whatsapp_started)
            .field("whatsapp_qr_exhausted", &self.whatsapp_qr_exhausted);
        #[cfg(feature = "signal-local")]
        out.field("signal_screen", &self.signal_screen)
            .field("signal_started", &self.signal_started)
            .field("signal_inbox_open", &self.signal_inbox_open)
            .field("signal_removed", &self.signal_removed)
            .field("signal_checking", &self.signal_checking);
        out.field("selected_signal_retained", &self.selected_signal_retained)
            .field("retained_publish", &self.retained_publish);
        out.finish_non_exhaustive()
    }
}

impl Default for Snapshot {
    fn default() -> Self {
        Self::new()
    }
}

/// One accepted Telegram send still waiting for its own end.
/// A send with no final answer, for the log at close. No text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnfinishedSend {
    pub protocol: ProtocolId,
    pub chat: String,
    pub stage: SendStage,
}

/// How far an unfinished send got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SendStage {
    /// The adapter has not answered `SendText` or `ResendMessage`.
    NoAnswer,
    /// Telegram only: TDLib took the message, the server has not confirmed
    /// it. TDLib keeps it in its message database.
    NotConfirmed,
}

struct TelegramHold {
    request: u64,
    chat: String,
    at: Instant,
    message_id: Option<String>,
    /// A retry. The row already shows "Sending…", so the status line does not.
    retry: bool,
}

/// A pending Telegram message id waiting for its own `SendAccepted`.
struct TelegramQueuedId {
    request: u64,
    chat: String,
    message_id: String,
}

/// How one Telegram send hold ends. Every end goes through [`Snapshot::end_telegram_send`].
enum TelegramSendEnd {
    /// This request: rejection, or the message id bound to it.
    Request(u64),
    /// Holds older than the 30 s send timeout.
    Expired(Instant),
    /// The Telegram session ended.
    All,
}

fn thread_key(
    protocol: ProtocolId,
    chat: &str,
    topic: Option<MessageTopic>,
) -> (ProtocolId, String, Option<MessageTopic>) {
    (protocol, chat.to_owned(), topic)
}

/// The most older pages that one click on a reply quote loads.
pub const REPLY_JUMP_PAGES: u8 = 5;

/// A reply quote click that waits for older pages.
#[derive(Debug, Clone)]
struct ReplyJump {
    thread: OlderPageKey,
    message_id: String,
    pages_left: u8,
}

fn older_key(
    protocol: ProtocolId,
    chat: &str,
    topic: Option<MessageTopic>,
) -> (ProtocolId, String, Option<MessageTopic>) {
    thread_key(protocol, chat, topic)
}

impl Snapshot {
    pub fn new() -> Self {
        let accounts = catalog()
            .into_iter()
            .map(|caps| AccountRow {
                caps,
                status: AdapterStatus::Stubbed,
                detail: caps.detail.to_string(),
                state: AccountState::Unlinked,
                helper: None,
                signal_retained: None,
                store_access: None,
                retained_read: RetainedRead::Readable,
            })
            .collect();
        Self {
            accounts,
            conversations: HashMap::new(),
            messages: HashMap::new(),
            topics: HashMap::new(),
            selected_protocol: ProtocolId::Telegram,
            selected_conversation: None,
            selected_topic: None,
            focused_row: None,
            notice_review: None,
            search: String::new(),
            auth: AuthScreen::Idle,
            telegram_api_id: String::new(),
            telegram_api_hash: String::new(),
            telegram_phone: String::new(),
            telegram_code: String::new(),
            telegram_2fa: String::new(),
            error: None,
            status_text: SIGN_IN_PROMPT.into(),
            ready_status: None,
            failure_status: None,
            telegram_send_unconfirmed: Vec::new(),
            telegram_unbound_messages: Vec::new(),
            outbox_read: HashMap::new(),
            compose: String::new(),
            auth_busy: false,
            auth_notice: None,
            auth_rejection: None,
            code_via: None,
            telegram_authorized: false,
            resume: Resume::Waiting,
            resume_unmarked: false,
            keychain_wait_started: None,
            keychain_failed: false,
            keychain_retry: false,
            store_key_retry_due: false,
            app_keychain_ready: false,
            chat_list_loading: HashSet::new(),
            history_loading: HashSet::new(),
            older_loading: HashSet::new(),
            list_on_link: HashSet::new(),
            older_at_start: HashSet::new(),
            older_retry: HashMap::new(),
            notices: HashMap::new(),
            viewed: None,
            window_focused: true,
            timed_out: Vec::new(),
            timeout_error: None,

            open_on_link: None,
            sessions: HashSet::new(),
            extra_visible: HashSet::new(),
            #[cfg(any(test, feature = "test-support"))]
            hidden_for_test: HashSet::new(),
            older_note: HashSet::new(),
            unreadable: HashMap::new(),
            open_unreadable: HashMap::new(),
            history_resume: HashMap::new(),
            history_asked: HashMap::new(),
            scroll_to_selected: false,
            scroll_to_focused: false,
            scroll_to_message: None,
            reply_jump: None,
            seen_visible_ids: Vec::new(),
            drafts: HashMap::new(),
            replying: None,
            editing: None,
            selection: None,
            forwarding: None,
            typing: crate::typing::Typing::default(),
            attachments: HashMap::new(),
            attachment_note: None,
            rejected_bodies: HashMap::new(),
            focus_compose: false,
            stopped: HashSet::new(),
            sends: SendTracker::default(),
            #[cfg(any(feature = "whatsapp-web", feature = "signal-local"))]
            next_pairing_generation: 1,
            #[cfg(any(feature = "whatsapp-web", feature = "signal-local"))]
            pairing_generation: HashMap::new(),
            data_reset: None,
            api_source: TelegramApiSource::from_build(),
            pending: Vec::new(),
            telegram_login_account: ProtocolId::Telegram,
            whatsapp_link_account: ProtocolId::WhatsApp,
            signal_link_account: ProtocolId::Signal,
            photo_downloads: HashSet::new(),
            file_downloads: HashSet::new(),
            keychain_flush: false,
            ended_accounts: Vec::new(),
            helper_errors: HashMap::new(),
            #[cfg(feature = "whatsapp-web")]
            whatsapp_screen: WhatsAppScreen::Hidden,
            #[cfg(feature = "whatsapp-web")]
            whatsapp_phone: String::new(),
            #[cfg(feature = "whatsapp-web")]
            whatsapp_phone_note: None,
            #[cfg(feature = "whatsapp-web")]
            whatsapp_qr: None,
            #[cfg(feature = "whatsapp-web")]
            whatsapp_pair_code: None,
            #[cfg(feature = "whatsapp-web")]
            whatsapp_qr_exhausted: false,
            #[cfg(feature = "whatsapp-web")]
            whatsapp_started: false,
            #[cfg(feature = "whatsapp-web")]
            whatsapp_risk_acknowledged: false,
            #[cfg(feature = "signal-local")]
            signal_screen: SignalScreen::Hidden,
            #[cfg(feature = "signal-local")]
            signal_qr: None,
            #[cfg(feature = "signal-local")]
            signal_started: false,
            #[cfg(feature = "signal-local")]
            signal_inbox_open: false,
            #[cfg(feature = "signal-local")]
            signal_notice_acknowledged: false,
            #[cfg(feature = "signal-local")]
            signal_notice_forget: false,
            #[cfg(feature = "signal-local")]
            signal_removed: false,
            #[cfg(feature = "signal-local")]
            signal_checking: false,
            selected_signal_retained: None,
            split: None,
            retained_publish: None,
        }
    }

    /// Use these Telegram API credentials, for example the invented ones of
    /// the demo (#120).
    pub(crate) fn set_api_source(&mut self, source: TelegramApiSource) {
        self.api_source = source;
    }

    #[cfg(test)]
    pub fn with_api_source(source: TelegramApiSource) -> Self {
        let mut snapshot = Self::new();
        snapshot.api_source = source;
        snapshot
    }

    pub fn apply(&mut self, event: AdapterEvent) {
        match event {
            AdapterEvent::FromAccount { account, event } => {
                self.apply_from_account(account, *event);
            }
            // An event with no protocol field is from account 0.
            event => match self.event_flow_account(&event) {
                Some(target) => self.apply_from_account(target.first_account(), event),
                None => self.apply_unchecked(event),
            },
        }
        // The edited message left the thread, for example by a delete or
        // an unlink: the draft comes back.
        if self.editing.is_some() && self.edit_target().is_none() {
            self.cancel_edit();
        }
    }

    fn apply_unchecked(&mut self, event: AdapterEvent) {
        // Inbox events come only while the account is linked. One that
        // arrives while it is not is from a cancelled or ended client, for
        // example queued before Cancel: drop it (PR #49 review). Adapters
        // send `Account { Linked }` before their first inbox event.
        if let Some(protocol) = event.inbox_protocol()
            && !self.has_session(protocol)
            && !Self::retained_signal_inbox(&event)
        {
            return;
        }
        match event {
            AdapterEvent::Status {
                protocol,
                status,
                detail,
            } => {
                // A retained-history line is not a status of the live account.
                // It does not change the link state or the session key.
                if protocol.is(ProtocolKind::Signal)
                    && protocol.slot() != 0
                    && is_signal_retained_line(&detail)
                {
                    // The retained history of another Signal account is not
                    // shown (ADR 0014).
                } else if protocol == ProtocolId::Signal
                    && detail == thinwire_protocol::SIGNAL_PREVIOUS_ACCOUNT
                {
                    // A different Signal number is linked. Drop the live chats of
                    // the previous number. The retained rows already hold them.
                    self.retire_live_signal_chats();
                } else if protocol.is(ProtocolKind::Signal)
                    && detail == thinwire_protocol::SIGNAL_RETAINED_RESET
                {
                    // Keep the rows. Each store sends its chats when that
                    // store is open. A slot that this publish does not send
                    // is dropped when the publish settles.
                    self.retained_publish = Some(HashSet::new());
                } else if protocol == ProtocolId::Signal
                    && detail == thinwire_protocol::SIGNAL_RETAINED_SETTLED
                {
                    if let Some(published) = self.retained_publish.take() {
                        self.drop_retained_slots_except(&published);
                    }
                    self.settle_retained_signal_selection();
                } else if protocol == ProtocolId::Signal
                    && let Some(slot) = thinwire_protocol::signal_retained_unreadable_slot(&detail)
                {
                    // The store is still on disk. Keep the row. A missing
                    // slot sends nothing here, so settle still drops that row.
                    self.keep_unreadable_retained_slot(slot);
                } else {
                    // A saved account never hears "did not link". That sentence is
                    // for a first link only.
                    let saved = self.has_session(protocol);
                    let detail = if saved && detail.contains("did not link") {
                        without_did_not_link(&detail)
                    } else {
                        detail
                    };
                    // The status line does not end a session. A failed move of a
                    // saved account puts the live row back to Linked: the files
                    // stayed. A previous Signal number is a separate row.
                    if let Some(row) = self.live_account_mut(protocol) {
                        row.status = status;
                        row.detail = detail.clone();
                        // A new status replaces the key sentence and hides Retry.
                        // A failed move is the exception: the files stayed, so
                        // the row stays linked and Retry tries the move again.
                        row.store_access = None;
                        if saved && stores_left_in_place(&detail) {
                            row.state = AccountState::Linked;
                            row.store_access = Some(StoreAccessReason::MoveFailed);
                        }
                    }
                    #[cfg(feature = "signal-local")]
                    if self.signal_screen_of(protocol) {
                        if status == AdapterStatus::Ready {
                            self.signal_inbox_open = true;
                        }
                        // Any other Signal line replaces the check. Removal uses
                        // its own flag, set below, and does not keep this one.
                        self.signal_checking = detail == thinwire_protocol::SIGNAL_LINK_CHECK;
                    }
                    #[cfg(feature = "whatsapp-web")]
                    if self.whatsapp_screen_of(protocol)
                        && status == AdapterStatus::Error
                        && detail == thinwire_protocol::WHATSAPP_QR_EXHAUSTED
                        && self.whatsapp_started
                    {
                        // The codes are used up. Drop the payload so the pair
                        // screen cannot draw it, and show the expired notice.
                        self.whatsapp_qr_exhausted = true;
                        self.whatsapp_qr = None;
                    }
                    if matches!(status, AdapterStatus::Error | AdapterStatus::Refused) {
                        // A failed load does not send its end event. Stop the spinners.
                        self.stop_spinners(protocol, None);
                        #[cfg(feature = "signal-local")]
                        if self.signal_screen_of(protocol)
                            && detail == thinwire_protocol::SIGNAL_DEVICE_REMOVED
                        {
                            // The row and the open chat show this line. No popup.
                            self.signal_removed = true;
                        } else if self.signal_screen_of(protocol) && self.signal_started {
                            self.note_signal_failure(&detail);
                        }
                    }
                    // Crate / feature jargon stays on the account row and in logs.
                    // Chrome surfaces Telegram errors and Ready operational copy only.
                    if protocol.is(ProtocolKind::Telegram)
                        && matches!(
                            status,
                            AdapterStatus::Error | AdapterStatus::Refused | AdapterStatus::Ready
                        )
                    {
                        self.ready_status =
                            (status == AdapterStatus::Ready).then(|| detail.clone());
                        self.failure_status =
                            (status != AdapterStatus::Ready).then(|| detail.clone());
                        // A status line never ends a send hold: its text names no
                        // request. The bound row's final delivery does (#168 item 16).
                        self.status_text = detail;
                        if matches!(status, AdapterStatus::Error | AdapterStatus::Refused) {
                            if self.auth != AuthScreen::Idle {
                                self.auth_busy = false;
                            }
                            self.resume = Resume::Settled;
                        }
                    }
                }
            }
            AdapterEvent::ConversationUpsert { conversation }
                if conversation.protocol.is(ProtocolKind::Signal)
                    && conversation.protocol.slot() != 0
                    && thinwire_protocol::signal_retained_slot(&conversation.id).is_some() =>
            {
                // The retained history of another Signal account is not
                // shown (ADR 0014).
                tracing::debug!(account = %conversation.protocol, "a retained Signal chat was dropped");
            }
            AdapterEvent::ConversationUpsert { mut conversation } => {
                if conversation.protocol.is(ProtocolKind::Signal)
                    && let Some(slot) = thinwire_protocol::signal_retained_slot(&conversation.id)
                {
                    // The previous number cannot send. The helper says so,
                    // and the core keeps that even if an event forgets it.
                    conversation.writable = false;
                    self.note_retained_slot(slot);
                }
                if self.window_focused
                    && !conversation.has_topics
                    && self.chat_is_viewed(conversation.protocol, &conversation.id)
                {
                    conversation.unread = 0;
                }
                let protocol = conversation.protocol;
                let list = self.conversations.entry(protocol).or_default();
                if let Some(existing) = list.iter_mut().find(|row| row.id == conversation.id) {
                    // A title or unread update keeps a download of the same file.
                    // A new file id drops the old path so the row loads again.
                    if existing.avatar.file_id == conversation.avatar.file_id {
                        if conversation.avatar.path.is_none() {
                            conversation.avatar.path.clone_from(&existing.avatar.path);
                        }
                        if conversation.avatar.bytes.is_none() {
                            conversation.avatar.bytes.clone_from(&existing.avatar.bytes);
                        }
                        if existing.avatar.failed {
                            conversation.avatar.failed = true;
                        }
                        if existing.avatar.requested {
                            conversation.avatar.requested = true;
                        }
                    }
                    *existing = conversation;
                } else {
                    list.push(conversation);
                }
                sort_conversations(list);
                // A message may move a row. The open chat stays the one the
                // user is reading. This fallback only fills an empty selection.
                self.ensure_conversation_selection();
                self.sync_focused_row(FocusFollow::Upsert);
            }
            AdapterEvent::TopicUpsert { topic } => self.upsert_topic(topic),
            AdapterEvent::MessageDelivery {
                protocol,
                conversation_id,
                message_id,
                delivery,
            } => {
                self.set_delivery(protocol, &conversation_id, &message_id, delivery);
                if protocol.is(ProtocolKind::Telegram)
                    && matches!(delivery, Delivery::Sent | Delivery::Read | Delivery::Failed)
                {
                    self.release_telegram_for_message(&message_id);
                }
            }
            AdapterEvent::OutboxRead {
                protocol,
                conversation_id,
                last_read_message_id,
            } => self.note_outbox_read(protocol, &conversation_id, last_read_message_id),
            AdapterEvent::SendAccepted {
                protocol,
                conversation_id,
                request,
            } => self.note_send_accepted(protocol, &conversation_id, request),
            AdapterEvent::SendRejected {
                protocol,
                conversation_id,
                request,
            } => self.fail_unaccepted_send(protocol, &conversation_id, request),
            AdapterEvent::Stopped { protocol } => {
                self.stopped.insert(protocol);
            }
            AdapterEvent::Account { protocol, state } => self.set_account(protocol, state),
            AdapterEvent::Helper { protocol, state } => self.set_helper(protocol, state),
            AdapterEvent::CommandFailed {
                protocol,
                conversation_id,
                detail,
            } => self.command_failed(protocol, conversation_id.as_deref(), &detail),
            AdapterEvent::Notice { protocol, text } => {
                self.notices.insert(protocol, text);
            }
            // Internal: the host unwraps stamped login events in poll_events.
            AdapterEvent::Login { .. } => {}
            // `apply` unwraps it.
            AdapterEvent::FromAccount { account, event } => {
                self.apply_from_account(account, *event);
            }
            AdapterEvent::OlderHistoryLoaded {
                protocol,
                conversation_id,
                before_message_id,
                more,
                note,
                topic,
                store_cursor,
            } => {
                // Every protocol: a failure sets the thread's note, a success
                // clears it. The thread shows plain text only. The topic is
                // part of the key, so an answer for topic A does not end topic B.
                let key = older_key(protocol, &conversation_id, topic);
                let failed = note.is_some();
                match note {
                    Some(detail) => {
                        tracing::debug!(?protocol, %detail, "older page did not load");
                        self.older_note.insert(key.clone());
                    }
                    None => {
                        self.older_note.remove(&key);
                    }
                }
                self.older_loading.remove(&key);
                // A failed page keeps the open run and the store cursor, so
                // the retry sends the same read. The asked timestamp is the
                // in-flight cursor; leaving it set makes that retry look like
                // a cursor that did not move. A success closes or continues
                // the run, and a success that does not move still drops it.
                let continued = if failed {
                    self.history_asked.remove(&key);
                    false
                } else if let Some(cursor) = store_cursor {
                    self.note_unreadable(UnreadablePage {
                        protocol,
                        chat: &conversation_id,
                        topic,
                        request_before: &before_message_id,
                        cursor,
                        more,
                        initial: false,
                    })
                } else {
                    self.close_open_run(&key);
                    self.history_resume.remove(&key);
                    false
                };
                if continued {
                    return;
                }
                let oldest = self
                    .message_bucket(protocol, &conversation_id, &before_message_id)
                    .and_then(|bucket| self.messages.get(&bucket))
                    .and_then(|list| list.first())
                    .map(|row| row.id.as_str());
                if !more {
                    self.older_retry.remove(&key);
                    self.older_at_start.insert(key);
                } else if oldest == Some(before_message_id.as_str()) {
                    // Nothing older came: do not ask this anchor at once.
                    // A repeat on the same anchor waits longer (#67).
                    let tries = match self.older_retry.get(&key) {
                        Some((anchor, _, tries)) if *anchor == before_message_id => tries + 1,
                        _ => 1,
                    };
                    self.older_retry
                        .insert(key, (before_message_id, Instant::now(), tries));
                } else {
                    self.older_retry.remove(&key);
                }
                self.continue_reply_jump();
            }
            AdapterEvent::ChatListLoaded { protocol } => {
                self.chat_list_loading.remove(&protocol);
            }
            AdapterEvent::HistoryLoaded {
                protocol,
                conversation_id,
                store_cursor,
            } => {
                let key = (protocol, conversation_id);
                // The history came during a reconnect: Linked need not load
                // the chat again (Codex on #111).
                if self.open_on_link.as_ref() == Some(&key) {
                    self.open_on_link = None;
                }
                self.history_loading.remove(&key);
                if let Some(cursor) = store_cursor {
                    let chat = key.1.clone();
                    let more =
                        cursor.resume || self.oldest_message_id(protocol, &chat, None).is_some();
                    self.note_unreadable(UnreadablePage {
                        protocol,
                        chat: &chat,
                        topic: None,
                        request_before: "",
                        cursor,
                        more,
                        initial: true,
                    });
                }
            }
            AdapterEvent::ConversationRemoved { protocol, id } => {
                self.remove_conversation(protocol, &id);
            }
            AdapterEvent::MessageReceived { message } => {
                if message.arrival == thinwire_protocol::Arrival::Live && !message.outbound {
                    let thread = (
                        message.protocol,
                        message.conversation_id.clone(),
                        message.topic,
                    );
                    self.typing.message_from(&thread, &message.sender);
                }
                let before =
                    self.delivery_of(message.protocol, &message.conversation_id, &message.id);
                self.note_delivery(before, &message);
                self.observe_telegram_message(&message, None);
                self.note_live_topic(&message);
                self.upsert_message(message);
            }
            AdapterEvent::MessageReplaced {
                protocol,
                conversation_id,
                old_id,
                message,
            } => {
                let before = self.delivery_of(protocol, &conversation_id, &old_id);
                self.note_delivery(before, &message);
                self.observe_telegram_message(&message, Some(&old_id));
                if protocol == message.protocol {
                    self.remove_message(protocol, &conversation_id, &old_id);
                }
                self.upsert_message(message);
            }
            AdapterEvent::MessageBody {
                protocol,
                conversation_id,
                message_id,
                body,
            } => {
                self.patch_message_body(protocol, &conversation_id, &message_id, body);
                self.mark_edited(protocol, &conversation_id, &message_id);
            }
            AdapterEvent::MessageEdited {
                protocol,
                conversation_id,
                message_id,
            } => self.mark_edited(protocol, &conversation_id, &message_id),
            AdapterEvent::Typing {
                protocol,
                conversation_id,
                topic,
                sender_id,
                sender,
                typing,
            } => {
                let _ = self.typing.note(
                    (protocol, conversation_id, topic),
                    sender_id,
                    sender,
                    typing,
                    Instant::now(),
                );
            }
            AdapterEvent::MessageMedia {
                protocol,
                conversation_id,
                message_id,
                body,
                links,
                photo,
                audio,
                video,
                file,
            } => self.patch_message_media(
                protocol,
                &conversation_id,
                &message_id,
                body,
                links,
                photo,
                audio,
                video,
                file,
            ),
            AdapterEvent::MessageLinks {
                protocol,
                conversation_id,
                message_id,
                links,
            } => self.patch_message_links(protocol, &conversation_id, &message_id, links),
            AdapterEvent::PhotoReady {
                protocol,
                file_id,
                file,
            } => self.photo_ready(protocol, file_id, file),
            AdapterEvent::PhotoFailed { protocol, file_id } => self.photo_failed(protocol, file_id),
            AdapterEvent::VoiceReady {
                protocol,
                file_id,
                path,
            } => self.voice_ready(protocol, file_id, path),
            AdapterEvent::VoiceFailed {
                protocol,
                file_id,
                stop,
            } => self.voice_failed(protocol, file_id, stop),
            AdapterEvent::VideoReady {
                protocol,
                file_id,
                path,
            } => self.video_ready(protocol, file_id, path),
            AdapterEvent::VideoFailed { protocol, file_id } => self.video_failed(protocol, file_id),
            AdapterEvent::FileReady {
                protocol,
                file_id,
                path,
            } => self.file_ready(protocol, file_id, path),
            AdapterEvent::FileFailed { protocol, file_id } => self.file_failed(protocol, file_id),
            AdapterEvent::MessageReactions {
                protocol,
                conversation_id,
                message_id,
                reactions,
            } => {
                if let Some(message) = self.message_mut(protocol, &conversation_id, &message_id) {
                    message.reactions = reactions;
                }
            }
            AdapterEvent::ChatAvatar {
                protocol,
                conversation_id,
                bytes,
            } => {
                if let Some(row) = self
                    .conversations
                    .get_mut(&protocol)
                    .and_then(|rows| rows.iter_mut().find(|row| row.id == conversation_id))
                {
                    row.avatar.bytes = Some(bytes);
                    row.avatar.failed = false;
                }
            }
            AdapterEvent::MessagesRemoved {
                protocol,
                conversation_id,
                message_ids,
            } => self.remove_messages(protocol, &conversation_id, &message_ids),
            AdapterEvent::TelegramAuth { phase } => self.apply_telegram_phase(phase),
            AdapterEvent::TelegramAuthRejected { error } => {
                self.auth_rejection = Some(error);
            }
            AdapterEvent::TelegramSessionEnded => self.end_telegram_session(),
            AdapterEvent::AccountEnded { protocol } => {
                self.ended_accounts.push(protocol);
                if protocol == ProtocolId::Signal {
                    // The account is gone. An open retained publish must not
                    // settle afterwards and drop a row that is still on disk.
                    self.retained_publish = None;
                }
                #[cfg(feature = "signal-local")]
                if self.signal_screen_of(protocol) {
                    self.signal_removed = true;
                    self.signal_checking = false;
                    // The row and the open chat show the removal. The strip
                    // must not keep the link success line, and it must not
                    // repeat that sentence.
                    self.clear_linked_signal_status();
                }
            }
            AdapterEvent::TelegramDataReset { moved_to } => {
                self.data_reset = Some(moved_to);
            }
            AdapterEvent::TelegramCodeSent { via } => {
                self.code_via = Some(via);
            }
            AdapterEvent::FlushSecrets => {
                self.keychain_flush = true;
            }
            AdapterEvent::WhatsAppQr { code, generation } => {
                #[cfg(feature = "whatsapp-web")]
                if self.current_pairing(ProtocolId::WhatsApp, generation) {
                    self.whatsapp_qr = Some(code.reveal().to_string());
                    self.whatsapp_qr_exhausted = false;
                }
                #[cfg(not(feature = "whatsapp-web"))]
                {
                    let _ = (code, generation);
                }
            }
            AdapterEvent::WhatsAppPairCode { code, generation } => {
                #[cfg(feature = "whatsapp-web")]
                if self.current_pairing(ProtocolId::WhatsApp, generation) {
                    self.whatsapp_pair_code = Some(code.reveal().to_string());
                }
                #[cfg(not(feature = "whatsapp-web"))]
                {
                    let _ = (code, generation);
                }
            }
            AdapterEvent::StoreAccess { protocol, reason } => {
                // A saved account keeps its session on a denied read. A wrong
                // key moved the store files aside, so the row leaves Linked.
                // A first link that cannot read the key is not a saved account.
                let saved = self.has_session(protocol);
                let detail = store_access_detail(protocol, reason, saved);
                let leave = match reason {
                    StoreAccessReason::WrongKey => {
                        saved
                            || self.accounts.iter().any(|row| {
                                row.caps.id == protocol && row.state != AccountState::Unlinked
                            })
                    }
                    StoreAccessReason::KeychainDenied => !saved,
                    StoreAccessReason::MoveFailed => false,
                };
                if leave {
                    self.set_account(protocol, AccountState::Unlinked);
                } else if saved
                    && matches!(
                        reason,
                        StoreAccessReason::KeychainDenied | StoreAccessReason::MoveFailed
                    )
                {
                    // A helper may have marked a saved account Linking while
                    // it read the key. The account is still linked.
                    // A previous Signal number does not take this update.
                    if let Some(row) = self.live_account_mut(protocol) {
                        row.state = AccountState::Linked;
                    }
                }
                if let Some(row) = self.live_account_mut(protocol) {
                    row.status = AdapterStatus::Error;
                    row.detail = detail.to_string();
                    row.store_access = Some(reason);
                }
                // A saved account, and the app keychain was not settled yet.
                // One later pump sends RetryStoreKey when that read succeeds.
                // A denial after the keychain is ready waits for the user's
                // Retry click. A first link has no session, so it does not
                // resume itself.
                if saved && reason == StoreAccessReason::KeychainDenied && !self.app_keychain_ready
                {
                    self.store_key_retry_due = true;
                }
            }
            AdapterEvent::SignalQr { code, generation } => {
                #[cfg(feature = "signal-local")]
                if self.current_pairing(ProtocolId::Signal, generation) {
                    self.signal_qr = Some(code.reveal().to_string());
                }
                #[cfg(not(feature = "signal-local"))]
                {
                    let _ = (code, generation);
                }
            }
        }
    }

    /// Queue one command for the worker. The core sends it on the next flush.
    pub(crate) fn queue(&mut self, command: AdapterCommand) {
        self.push(command);
    }

    pub fn take_commands(&mut self) -> Vec<AdapterCommand> {
        std::mem::take(&mut self.pending)
    }

    // region: accounts

    /// Add the row of `account` after the other accounts of its protocol
    /// (ADR 0014). It copies the capabilities of account 0. `false` for
    /// account 0, for a known account, and for a protocol with no row.
    pub fn add_account_row(&mut self, account: ProtocolId) -> bool {
        if account.slot() == 0 || self.accounts.iter().any(|row| row.caps.id == account) {
            return false;
        }
        let Some(first) = self
            .accounts
            .iter()
            .find(|row| row.caps.id == account.first_account() && row.signal_retained.is_none())
        else {
            return false;
        };
        let mut caps = first.caps;
        caps.id = account;
        let row = AccountRow {
            caps,
            status: AdapterStatus::Stubbed,
            detail: caps.detail.to_string(),
            state: AccountState::Unlinked,
            helper: None,
            signal_retained: None,
            retained_read: RetainedRead::Readable,
            store_access: None,
        };
        let at = self
            .accounts
            .iter()
            .rposition(|row| row.caps.id.same_protocol(account))
            .map_or(self.accounts.len(), |at| at + 1);
        self.accounts.insert(at, row);
        true
    }

    /// The accounts in the order of their rows, one time each.
    #[must_use]
    pub fn account_order(&self) -> Vec<ProtocolId> {
        let mut order = Vec::new();
        for row in &self.accounts {
            if !order.contains(&row.caps.id) {
                order.push(row.caps.id);
            }
        }
        order
    }

    /// Put the rows in the saved order of the user. An account that is not
    /// in `order` goes after the others, in the order of the build. The
    /// rows of one account stay together.
    pub fn apply_account_order(&mut self, order: &[ProtocolId]) {
        self.accounts.sort_by_key(|row| {
            order
                .iter()
                .position(|account| *account == row.caps.id)
                .unwrap_or(usize::MAX)
        });
    }

    /// Move `account` one place up or down in the sidebar, past the next
    /// account that the sidebar shows. `false`: it is at the end already,
    /// or the sidebar does not show it.
    pub fn move_account(&mut self, account: ProtocolId, up: bool) -> bool {
        let mut order = self.account_order();
        let shown: Vec<usize> = (0..order.len())
            .filter(|at| self.shows_in_switcher(order[*at]))
            .collect();
        let Some(at) = shown.iter().position(|index| order[*index] == account) else {
            return false;
        };
        let to = if up { at.checked_sub(1) } else { Some(at + 1) };
        let Some(to) = to.filter(|to| *to < shown.len()) else {
            return false;
        };
        order.swap(shown[at], shown[to]);
        self.apply_account_order(&order);
        true
    }

    /// Remove the row of `account` and its chats. Account 0 stays. The
    /// keychain values and the files of the account stay too.
    pub fn remove_account_row(&mut self, account: ProtocolId) {
        if account.slot() == 0 {
            return;
        }
        self.end_session(account);
        self.accounts.retain(|row| row.caps.id != account);
        self.stopped.remove(&account);
        if self.selected_protocol == account {
            self.selected_protocol = account.first_account();
            self.set_selected_conversation(None);
        }
    }

    /// The user unlinks `account`: queue the unlink for its adapter. The
    /// row stays linked until the adapter reports the end. `false` for an
    /// account that is not linked: nothing is queued.
    pub(crate) fn unlink_account(&mut self, account: ProtocolId) -> bool {
        if !self.protocol_linked(account) {
            return false;
        }
        self.status_text = format!("Unlinking {}…", account.label());
        self.pending
            .push(AdapterCommand::Unlink { protocol: account });
        true
    }

    /// `account` left the linked state after an unlink of the user. The
    /// gate acceptance of its pairing ends in memory. `forget_gate` also
    /// drops the saved acceptance of the Signal notice. A Telegram account
    /// of the login screens is not signed in any more.
    pub(crate) fn finish_unlink(&mut self, account: ProtocolId, forget_gate: bool) {
        if self.telegram_login_account == account {
            self.telegram_authorized = false;
            self.resume = Resume::Settled;
        }
        #[cfg(feature = "whatsapp-web")]
        if self.whatsapp_screen_of(account) && !self.whatsapp_gate_open() {
            self.whatsapp_risk_acknowledged = false;
            self.whatsapp_started = false;
        }
        #[cfg(feature = "signal-local")]
        if account.is(ProtocolKind::Signal) && forget_gate {
            self.signal_notice_acknowledged = false;
            self.signal_notice_forget = true;
        }
        #[cfg(not(feature = "signal-local"))]
        let _ = forget_gate;
        self.status_text = format!("{} is unlinked.", account.label());
    }

    /// The lowest free account number of `kind` after account 0. `None`
    /// when every number up to [`thinwire_protocol::MAX_ACCOUNT_SLOT`] is in
    /// use.
    #[must_use]
    pub fn next_account(&self, kind: ProtocolKind) -> Option<ProtocolId> {
        (1..=thinwire_protocol::MAX_ACCOUNT_SLOT).find_map(|slot| {
            let account = ProtocolId::account(kind, slot)?;
            (!self.accounts.iter().any(|row| row.caps.id == account)).then_some(account)
        })
    }

    /// The accounts of this build, account 0 of each protocol first.
    #[must_use]
    pub fn account_ids(&self) -> Vec<ProtocolId> {
        self.accounts
            .iter()
            .filter(|row| row.signal_retained.is_none())
            .map(|row| row.caps.id)
            .collect()
    }

    /// The Telegram account of the login screens.
    #[must_use]
    pub const fn telegram_login_account(&self) -> ProtocolId {
        self.telegram_login_account
    }

    /// The WhatsApp account of the gate and the pair screen.
    #[must_use]
    pub const fn whatsapp_link_account(&self) -> ProtocolId {
        self.whatsapp_link_account
    }

    /// The Signal account of the notice and the link screen.
    #[must_use]
    pub const fn signal_link_account(&self) -> ProtocolId {
        self.signal_link_account
    }

    /// The WhatsApp gate and pair screen are for `protocol`.
    #[cfg(feature = "whatsapp-web")]
    fn whatsapp_screen_of(&self, protocol: ProtocolId) -> bool {
        protocol == self.whatsapp_link_account
    }

    /// The Signal notice and link screen are for `protocol`.
    #[cfg(feature = "signal-local")]
    fn signal_screen_of(&self, protocol: ProtocolId) -> bool {
        protocol == self.signal_link_account
    }

    /// The account is linked.
    #[must_use]
    pub fn protocol_is_linked(&self, account: ProtocolId) -> bool {
        self.protocol_linked(account)
    }

    /// The account can take a new login: it is not linked and no login
    /// screen is open for its protocol.
    #[must_use]
    pub fn account_free(&self, account: ProtocolId) -> bool {
        if self.protocol_linked(account) || !self.account_surface_visible(account.first_account()) {
            return false;
        }
        !self.login_open(account.kind())
            && !(account == ProtocolId::Telegram && self.telegram_authorized)
    }

    /// A login screen of `kind` is open, for any account.
    #[must_use]
    pub fn login_open(&self, kind: ProtocolKind) -> bool {
        match kind {
            ProtocolKind::Telegram => self.auth != AuthScreen::Idle,
            #[cfg(feature = "whatsapp-web")]
            ProtocolKind::WhatsApp => self.whatsapp_gate_open(),
            #[cfg(feature = "signal-local")]
            ProtocolKind::Signal => self.signal_gate_open(),
            _ => false,
        }
    }

    /// Open the login screen of `account` (ADR 0014). A login of the same
    /// protocol that is open already stays.
    pub fn open_login(&mut self, account: ProtocolId, store: &SecretStore) {
        match account.kind() {
            // The login screens fill the center. Ready selects the account.
            ProtocolKind::Telegram => {
                self.begin_telegram_login(account, store);
                return;
            }
            ProtocolKind::WhatsApp => {
                #[cfg(feature = "whatsapp-web")]
                {
                    if self.whatsapp_gate_open() {
                        self.status_text = "Finish or cancel the open WhatsApp link first.".into();
                    } else {
                        self.whatsapp_link_account = account;
                        self.open_whatsapp_risk_gate();
                    }
                }
            }
            ProtocolKind::Signal => {
                #[cfg(feature = "signal-local")]
                {
                    if self.signal_gate_open() {
                        self.status_text = "Finish or cancel the open Signal link first.".into();
                    } else if account.slot() == 0 {
                        self.open_signal_notice();
                    } else {
                        // Another account shows the notice: its helper needs
                        // the acceptance. The saved acceptance of account 0
                        // stays.
                        self.signal_link_account = account;
                        self.reveal_signal_notice(false);
                    }
                }
            }
            ProtocolKind::Discord => {
                self.status_text = format!(
                    "Put the bot token of {} in the keychain as {}, then restart thinwire.",
                    account.label(),
                    account.scoped(thinwire_protocol::DISCORD_SECRET_BOT_TOKEN)
                );
            }
            ProtocolKind::Slack => {
                self.status_text = format!(
                    "Install the Slack app in the workspace of {} with Add Slack workspace.",
                    account.label()
                );
            }
        }
        self.select_protocol(account);
    }

    /// The Telegram login screens for `account`. Account 0 is Add account.
    /// Another account parks the login state of account 0 and gets it back
    /// when its login ends.
    fn begin_telegram_login(&mut self, account: ProtocolId, store: &SecretStore) {
        if account.slot() == 0 {
            if self.telegram_login_account.slot() == 0 {
                self.open_add_account(store);
            }
            return;
        }
        if self.auth != AuthScreen::Idle && self.telegram_login_account != account {
            self.status_text = "Finish or cancel the open Telegram sign-in first.".into();
            return;
        }
        // Until the keychain read of this account ends, nothing is parked:
        // `open_telegram` shows the reason and opens no screen.
        if store.read_failed() || !store.attach_settled() {
            self.open_telegram(store);
            return;
        }
        self.telegram_login_account = account;
        self.telegram_authorized = false;
        self.resume = Resume::Settled;
        self.open_telegram(store);
    }

    /// The login of another Telegram account ended: the screens are for
    /// account 0 again.
    fn restore_telegram_login(&mut self) {
        if self.telegram_login_account.slot() == 0 {
            return;
        }
        self.telegram_login_account = ProtocolId::Telegram;
        self.telegram_authorized = self.protocol_linked(ProtocolId::Telegram);
        self.resume = Resume::Settled;
    }

    /// No login screen is for `account` any more. Its protocol's screens
    /// close when they were for it.
    pub fn end_login_of(&mut self, account: ProtocolId) {
        if self.telegram_login_account == account {
            self.auth = AuthScreen::Idle;
            self.auth_busy = false;
            self.restore_telegram_login();
        }
        #[cfg(feature = "whatsapp-web")]
        if self.whatsapp_link_account == account {
            if self.whatsapp_gate_open() {
                self.whatsapp_screen = WhatsAppScreen::Hidden;
            }
            self.whatsapp_link_account = ProtocolId::WhatsApp;
        }
        #[cfg(feature = "signal-local")]
        if self.signal_link_account == account {
            if self.signal_gate_open() {
                self.signal_screen = SignalScreen::Hidden;
            }
            self.signal_link_account = ProtocolId::Signal;
        }
    }

    /// Resume the saved session of an account after account 0, with no
    /// screen. Only Telegram needs it: the other protocols connect when
    /// their adapter starts or when their token loads.
    pub fn resume_account(&mut self, account: ProtocolId, store: &SecretStore) {
        if !account.is(ProtocolKind::Telegram) || account.slot() == 0 || !tdlib_compiled() {
            return;
        }
        let saved = store.get(SecretKey::Session).ok().flatten().is_some();
        if !(saved || unmarked_database(store)) || !self.has_api_credentials(store) {
            return;
        }
        self.pending.push(
            AdapterCommand::TelegramAuth {
                step: TelegramAuthStep::ApiCredentials,
                epoch: 0,
            }
            .for_account(account),
        );
    }

    /// Start the helper of a saved WhatsApp or Signal account after account
    /// 0 with the accepted gate. No screen opens.
    pub(crate) fn resume_helper_account(&mut self, account: ProtocolId) {
        if account.slot() == 0 || self.helper_missing(account) {
            return;
        }
        let gate = match account.kind() {
            ProtocolKind::WhatsApp => AdapterCommand::WhatsAppAcknowledgeRisk,
            ProtocolKind::Signal => AdapterCommand::SignalAcknowledgeNotice,
            _ => return,
        };
        self.pending.push(gate.for_account(account));
    }

    // endregion: accounts

    /// Queue a command. A login step or a pairing command has no protocol
    /// field: it goes to the account of its screen (ADR 0014).
    fn push(&mut self, command: AdapterCommand) {
        let command = match self.flow_account(&command) {
            Some(account) => command.for_account(account),
            None => command,
        };
        self.pending.push(command);
    }

    /// The account of the screen that a login or pairing command belongs to.
    /// `None` for a command with a protocol field.
    fn flow_account(&self, command: &AdapterCommand) -> Option<ProtocolId> {
        match command {
            AdapterCommand::TelegramAuth { .. } => Some(self.telegram_login_account),
            AdapterCommand::WhatsAppAcknowledgeRisk
            | AdapterCommand::WhatsAppBeginLink { .. }
            | AdapterCommand::WhatsAppCancelLink => Some(self.whatsapp_link_account),
            AdapterCommand::SignalAcknowledgeNotice
            | AdapterCommand::SignalBeginLink { .. }
            | AdapterCommand::SignalCancelLink => Some(self.signal_link_account),
            _ => None,
        }
    }

    /// The account of the screen that a login or pairing event belongs to.
    /// `None` for an event with a protocol field, and for an event of a
    /// whole account (a secret flush).
    fn event_flow_account(&self, event: &AdapterEvent) -> Option<ProtocolId> {
        match event {
            AdapterEvent::TelegramAuth { .. }
            | AdapterEvent::TelegramAuthRejected { .. }
            | AdapterEvent::TelegramDataReset { .. }
            | AdapterEvent::TelegramSessionEnded
            | AdapterEvent::TelegramCodeSent { .. } => Some(self.telegram_login_account),
            AdapterEvent::WhatsAppQr { .. } | AdapterEvent::WhatsAppPairCode { .. } => {
                Some(self.whatsapp_link_account)
            }
            AdapterEvent::SignalQr { .. } => Some(self.signal_link_account),
            _ => None,
        }
    }

    /// An event of the adapter of `account`, with no protocol field. Only
    /// the account of the screen changes the screen. A secret flush counts
    /// for every account.
    fn apply_from_account(&mut self, account: ProtocolId, event: AdapterEvent) {
        match self.event_flow_account(&event) {
            Some(target) if target == account => self.apply_unchecked(event),
            // A Telegram account that is not on the login screens: its login
            // phases link or unlink its row only.
            Some(_) if account.is(ProtocolKind::Telegram) => {
                self.apply_background_telegram(account, event);
            }
            Some(_) => tracing::debug!(%account, "a login event of another account was dropped"),
            None => self.apply_unchecked(event),
        }
    }

    /// A login phase of a Telegram account that is not on the login
    /// screens, for example the resume of a saved session at start. Ready
    /// links its row and loads its chats. A request for a phone, a code, or
    /// a password means the session ended: the row unlinks and shows Link.
    fn apply_background_telegram(&mut self, account: ProtocolId, event: AdapterEvent) {
        let phase = match event {
            AdapterEvent::TelegramAuth { phase } => Some(phase),
            AdapterEvent::TelegramSessionEnded => None,
            _ => return,
        };
        match phase {
            Some(TelegramAuthPhase::Ready) => {
                if let Some(row) = self.live_account_mut(account) {
                    row.state = AccountState::Linked;
                    row.status = AdapterStatus::Ready;
                }
                if self.sessions.insert(account) {
                    self.chat_list_loading.insert(account);
                }
            }
            // The client did not start. A linked row stays linked.
            Some(TelegramAuthPhase::Failed | TelegramAuthPhase::Unavailable) => {
                if let Some(row) = self.live_account_mut(account) {
                    row.status = AdapterStatus::Error;
                }
            }
            // The saved session is gone.
            Some(
                TelegramAuthPhase::NeedPhone
                | TelegramAuthPhase::NeedCode
                | TelegramAuthPhase::NeedTwoFactor,
            )
            | None => {
                if self.has_session(account) {
                    self.end_session(account);
                }
                if let Some(row) = self.live_account_mut(account) {
                    row.state = AccountState::Unlinked;
                    row.status = AdapterStatus::Stubbed;
                    row.detail = ACCOUNT_SIGN_IN_AGAIN.into();
                }
                // The client must not wait for a phone number on no screen.
                if phase.is_some() {
                    self.push(AdapterCommand::Disconnect { protocol: account });
                }
            }
        }
    }

    #[must_use]
    pub fn take_keychain_flush(&mut self) -> bool {
        std::mem::take(&mut self.keychain_flush)
    }

    /// Accounts that ended for good since the last call. See
    /// `ended_accounts`.
    #[must_use]
    pub(crate) fn take_ended_accounts(&mut self) -> Vec<ProtocolId> {
        std::mem::take(&mut self.ended_accounts)
    }

    /// Start TDLib with no click when the keychain holds a saved session.
    ///
    /// Call once per frame. It acts only after the keychain read settles, and
    /// only once. The UI thread reads memory only; the command has no secret.
    pub fn poll_resume(&mut self, store: &SecretStore) {
        self.keychain_failed = store.read_failed();
        if !store.attach_settled() {
            self.keychain_wait_started.get_or_insert_with(Instant::now);
        }
        self.try_resume(store, tdlib_compiled());
    }

    /// Copy under the spinner while the keychain read runs. No bare spinner:
    /// after [`KEYCHAIN_SLOW_AFTER`] it asks the user to unlock the keychain.
    #[must_use]
    pub fn keychain_wait_text(&self, now: Instant) -> &'static str {
        match self.keychain_wait_started {
            Some(started) if now.saturating_duration_since(started) >= KEYCHAIN_SLOW_AFTER => {
                KEYCHAIN_WAITING
            }
            _ => KEYCHAIN_OPENING,
        }
    }

    /// Test hook: start the saved-session resume as a `telegram-tdlib` build
    /// does, also in a default test build.
    #[cfg(test)]
    pub(crate) fn resume_for_test(&mut self, store: &SecretStore) {
        self.try_resume(store, true);
    }

    fn try_resume(&mut self, store: &SecretStore, live: bool) {
        if self.resume != Resume::Waiting || !store.attach_settled() {
            return;
        }
        let saved_session = store.get(SecretKey::Session).ok().flatten().is_some();
        let unmarked = !saved_session && unmarked_database(store);
        if !live
            || !(saved_session || unmarked)
            || self.auth != AuthScreen::Idle
            || self.telegram_authorized
            || !self.has_api_credentials(store)
        {
            self.resume = Resume::Settled;
            return;
        }
        self.resume = Resume::Connecting;
        self.resume_unmarked = unmarked;
        self.queue_telegram_step(TelegramAuthStep::ApiCredentials);
        self.status_text = RESUME_CONNECTING.into();
    }

    #[must_use]
    pub fn center_view(&self) -> CenterView {
        if self.auth != AuthScreen::Idle {
            return CenterView::Auth;
        }
        // Any linked protocol shows the inbox, not only Telegram (shell plan 2).
        // A previous Signal number is local history. It opens with no session.
        if self.any_linked() || self.selected_signal_retained.is_some() {
            return CenterView::Thread;
        }
        if self.keychain_failed && !self.keychain_retry {
            return CenterView::KeychainFailed;
        }
        match self.resume {
            Resume::Waiting if tdlib_compiled() => CenterView::Resuming { connecting: false },
            Resume::Connecting => CenterView::Resuming { connecting: true },
            Resume::Waiting | Resume::Settled => CenterView::FirstRun,
        }
    }

    /// A linked Telegram account. The Telegram login screens use it.
    pub fn has_primary_account(&self) -> bool {
        self.protocol_linked(ProtocolId::Telegram)
    }

    /// At least one visible protocol has a session: the shell shows the
    /// inbox. A reconnect (`Linking` after `Linked`) keeps it shown.
    #[must_use]
    pub fn any_linked(&self) -> bool {
        self.accounts
            .iter()
            .any(|row| self.has_session(row.caps.id) && self.account_surface_visible(row.caps.id))
    }

    /// This protocol has a session: `Linked` now, or reconnecting after
    /// `Linked` (`Linking`). Its rows show and its inbox events and send
    /// answers apply. New commands wait for `Linked` (`protocol_linked`).
    #[must_use]
    pub fn has_session(&self, protocol: ProtocolId) -> bool {
        self.protocol_linked(protocol) || self.sessions.contains(&protocol)
    }

    /// The last note of this protocol, if any. It is not an error.
    #[must_use]
    pub fn notice(&self, protocol: ProtocolId) -> Option<&str> {
        self.notices.get(&protocol).map(String::as_str)
    }

    pub fn select_protocol(&mut self, protocol: ProtocolId) {
        if !self.account_surface_visible(protocol) {
            return;
        }
        // Park the draft under the old protocol before the protocol changes.
        self.set_selected_conversation(None);
        self.selected_protocol = protocol;
        // The live account of that protocol. A previous Signal number has
        // its own row.
        self.selected_signal_retained = None;
        self.focused_row = None;
        self.ensure_conversation_selection();
    }

    /// Open a previous Signal account. Sending stays off. The history stays.
    pub fn select_retained_signal(&mut self, slot: u32) {
        let known = self
            .accounts
            .iter()
            .any(|row| row.caps.id.is(ProtocolKind::Signal) && row.signal_retained == Some(slot));
        if !known || !self.account_surface_visible(ProtocolId::Signal) {
            return;
        }
        self.set_selected_conversation(None);
        self.selected_protocol = ProtocolId::Signal;
        self.selected_signal_retained = Some(slot);
        self.focused_row = None;
        self.ensure_conversation_selection();
    }

    /// What the pane with the focus shows now.
    fn pane_view(&self) -> PaneView {
        PaneView {
            protocol: self.selected_protocol,
            signal_retained: self.selected_signal_retained,
            conversation: self.selected_conversation.clone(),
            topic: self.selected_topic,
            search: self.search.clone(),
        }
    }

    /// The side of the pane with the focus. `None` is one pane.
    #[must_use]
    pub fn split(&self) -> Option<SplitSide> {
        self.split.as_ref().map(|split| split.active)
    }

    /// The account of the pane that does not have the focus.
    #[must_use]
    pub fn split_other_protocol(&self) -> Option<ProtocolId> {
        self.split.as_ref().map(|split| split.other.protocol)
    }

    /// The chat and topic of the pane that does not have the focus.
    #[must_use]
    pub fn split_other_chat(&self) -> Option<(ProtocolId, &str, Option<MessageTopic>)> {
        let other = &self.split.as_ref()?.other;
        Some((other.protocol, other.conversation.as_deref()?, other.topic))
    }

    /// Show `protocol` in a second pane, on the right, and give it the
    /// focus. The pane on the left keeps the account and the chat that are
    /// open now. With a split already open, the pane with the focus changes
    /// to `protocol`.
    pub fn open_split(&mut self, protocol: ProtocolId) {
        if !self.account_surface_visible(protocol) {
            return;
        }
        if self.split.is_none() {
            self.split = Some(Split {
                other: self.pane_view(),
                active: SplitSide::Right,
                peeked: None,
            });
            // Each pane has its own search.
            self.set_search(String::new());
        }
        self.select_protocol(protocol);
    }

    /// Go back to one pane: the pane with the focus stays.
    pub fn close_split(&mut self) {
        self.split = None;
    }

    /// Give the focus to the other pane. Its account, chat, topic, and
    /// search become the selection, with the same steps as a click on that
    /// account and that chat: the draft of each thread stays with it.
    pub fn focus_other_pane(&mut self) {
        let Some(split) = self.split.take() else {
            return;
        };
        let here = self.pane_view();
        let there = split.other;
        match there.signal_retained {
            Some(slot) => self.select_retained_signal(slot),
            None => self.select_protocol(there.protocol),
        }
        if self.selected_protocol == there.protocol
            && let Some(id) = there.conversation
            && self.conversation(there.protocol, &id).is_some()
        {
            let opened = there
                .topic
                .is_some_and(|topic| self.select_topic(id.clone(), topic));
            if !opened {
                self.select_conversation(id);
            }
        }
        self.set_search(there.search);
        self.split = Some(Split {
            other: here,
            active: split.active.other(),
            peeked: None,
        });
    }

    /// Close the split when the account of its other pane is gone.
    pub(crate) fn check_split(&mut self) {
        let Some(protocol) = self.split_other_protocol() else {
            return;
        };
        let listed = self.accounts.iter().any(|row| row.caps.id == protocol);
        if !listed || !self.account_surface_visible(protocol) {
            self.split = None;
        }
    }

    /// Change the selection to the other pane, or back, for one draw of the
    /// frontend. It moves no draft and sends no command: the second call
    /// puts every value back. The compose text of the other pane is the
    /// draft of its thread.
    pub(crate) fn peek_other_pane(&mut self) {
        let Some(mut split) = self.split.take() else {
            return;
        };
        let here = self.pane_view();
        let there = std::mem::replace(&mut split.other, here);
        self.selected_protocol = there.protocol;
        self.selected_signal_retained = there.signal_retained;
        self.selected_conversation = there.conversation;
        self.selected_topic = there.topic;
        self.search = there.search;
        match split.peeked.take() {
            Some((compose, focused_row)) => {
                self.compose = compose;
                self.focused_row = focused_row;
            }
            None => {
                let draft = self
                    .selected_conversation
                    .as_ref()
                    .and_then(|id| {
                        self.drafts
                            .get(&(self.selected_protocol, id.clone(), self.selected_topic))
                    })
                    .cloned()
                    .unwrap_or_default();
                split.peeked = Some((
                    std::mem::replace(&mut self.compose, draft),
                    self.focused_row.take(),
                ));
            }
        }
        self.split = Some(split);
    }

    /// The open account row is selected in the switcher.
    #[must_use]
    pub fn account_row_selected(&self, account: &AccountRow) -> bool {
        if self.selected_protocol != account.caps.id {
            return false;
        }
        if !account.caps.id.is(ProtocolKind::Signal) {
            return true;
        }
        self.selected_signal_retained == account.signal_retained
    }

    /// Why the open Signal chat cannot send, when it is a previous number.
    /// An unreadable store uses the history sentence.
    /// `None` for the live number and for every other protocol.
    #[must_use]
    pub fn signal_read_only_reason(&self) -> Option<&'static str> {
        if self.selected_protocol != ProtocolId::Signal {
            return None;
        }
        let slot = self.selected_signal_retained?;
        let unreadable = self
            .accounts
            .iter()
            .any(|row| row.signal_retained == Some(slot) && row.history_unreadable());
        if unreadable {
            Some(thinwire_protocol::SIGNAL_RETAINED_UNREADABLE)
        } else {
            Some(thinwire_protocol::SIGNAL_PREVIOUS_ACCOUNT)
        }
    }

    /// The open row could not be read.
    /// That row has no composer and no Send control.
    #[must_use]
    pub fn hides_composer(&self) -> bool {
        if self.selected_protocol != ProtocolId::Signal {
            return false;
        }
        let Some(slot) = self.selected_signal_retained else {
            return false;
        };
        self.accounts
            .iter()
            .any(|row| row.signal_retained == Some(slot) && row.history_unreadable())
    }

    /// Chip text for one previous Signal account.
    /// One previous account keeps the reason line. Two or more use
    /// "Previous account N". N is the slot. It is not a phone number and
    /// it is not an account id.
    /// A store that could not be read shows the history sentence.
    /// That sentence has no slot count.
    #[must_use]
    pub fn signal_retained_chip_label(&self, slot: u32) -> String {
        if self
            .accounts
            .iter()
            .any(|row| row.signal_retained == Some(slot) && row.history_unreadable())
        {
            return thinwire_protocol::SIGNAL_RETAINED_UNREADABLE.to_owned();
        }
        let count = self
            .accounts
            .iter()
            .filter(|row| row.caps.id.is(ProtocolKind::Signal) && row.signal_retained.is_some())
            .count();
        if count >= 2 {
            format!("Previous account {slot}")
        } else {
            thinwire_protocol::SIGNAL_PREVIOUS_ACCOUNT.to_owned()
        }
    }

    /// Open `id`. The inbox scrolls that row into view.
    /// A click, Enter on the highlight, and a notification click all come here.
    /// A later message does not: the list stays where the user left it.
    pub fn select_conversation(&mut self, id: String) {
        self.focused_row = Some(id.clone());
        let has_topics = self
            .conversation(self.selected_protocol, &id)
            .is_some_and(|row| row.has_topics);
        self.set_selection(Some(id), None);
        // The topic list has no compose field.
        self.focus_compose = !has_topics;
        self.scroll_to_selected = true;
        self.queue_open_chat();
    }

    /// Open one topic of the selected chat. A hidden topic is not a row.
    /// `false` when that topic is not loaded: the caller can open the chat.
    pub fn select_topic(&mut self, conversation_id: String, topic: MessageTopic) -> bool {
        let protocol = self.selected_protocol;
        let Some(row) = self
            .topics
            .get(&(protocol, conversation_id.clone()))
            .and_then(|rows| rows.iter().find(|row| row.topic == topic))
        else {
            return false;
        };
        if row.hidden {
            return false;
        }
        let closed = row.closed;
        let same_chat = self.selected_conversation.as_deref() == Some(conversation_id.as_str());
        if !same_chat {
            self.focused_row = Some(conversation_id.clone());
            self.scroll_to_selected = true;
        }
        self.set_selection(Some(conversation_id), Some(topic));
        self.focus_compose = !closed;
        self.queue_open_chat();
        true
    }

    /// Leave the open topic and show its topic list. The draft stays with
    /// the topic.
    pub fn close_open_topic(&mut self) {
        if self.selected_topic.is_none() {
            return;
        }
        let id = self.selected_conversation.clone();
        self.set_selection(id, None);
        self.focus_compose = false;
    }

    /// Move the keyboard highlight among the visible inbox rows.
    /// The open chat, its draft, and its messages stay as they are.
    /// The row an arrow would highlight. `None` when the highlight would not change.
    #[must_use]
    pub fn inbox_move_target(&self, delta: i32) -> Option<String> {
        if delta == 0 {
            return None;
        }
        let ids: Vec<String> = self
            .visible_conversations()
            .iter()
            .map(|row| row.id.clone())
            .collect();
        if ids.is_empty() {
            return None;
        }
        let current = self
            .focused_row
            .as_deref()
            .or(self.selected_conversation.as_deref())
            .and_then(|id| ids.iter().position(|row| row == id));
        let next = match current {
            Some(index) => (index as i32 + delta).clamp(0, ids.len() as i32 - 1) as usize,
            None if delta < 0 => ids.len() - 1,
            None => 0,
        };
        let id = ids[next].clone();
        (self.focused_row.as_deref() != Some(id.as_str())).then_some(id)
    }

    pub fn move_inbox_selection(&mut self, delta: i32) {
        let Some(id) = self.inbox_move_target(delta) else {
            return;
        };
        self.focused_row = Some(id);
        self.sync_focused_row(FocusFollow::Moved);
    }

    /// Point the highlight at a row that already has keyboard focus.
    /// The open chat stays as it is. The row is already on screen, so this does not scroll.
    pub fn focus_inbox_row(&mut self, id: String) {
        let visible = self.visible_conversations().iter().any(|row| row.id == id);
        if visible {
            self.focused_row = Some(id);
        }
    }

    /// Change the selected chat. The compose text stays with the chat it was typed in.
    /// True when this chat of this protocol is the selected one.
    #[must_use]
    pub fn is_selected_chat(&self, protocol: ProtocolId, chat: &str) -> bool {
        self.selected_protocol == protocol && self.selected_conversation.as_deref() == Some(chat)
    }

    /// Store unsent text of one chat. The selected chat keeps it in `compose`,
    /// another chat in its draft. Never "the selected chat" by default: the
    /// text stays with the chat it was typed in (PR #48 review).
    pub fn set_draft(
        &mut self,
        protocol: ProtocolId,
        chat: &str,
        topic: Option<MessageTopic>,
        text: String,
    ) {
        if self.is_selected_thread(protocol, chat, topic) {
            self.compose = text;
        } else if text.is_empty() {
            self.drafts.remove(&(protocol, chat.to_owned(), topic));
        } else {
            self.drafts.insert((protocol, chat.to_owned(), topic), text);
        }
    }

    /// The draft of the open thread changed: tell its chat that this
    /// account types, or that it stopped (#510). Nothing goes out for
    /// another thread, for an edit, for a protocol with no typing, or for a
    /// chat that this account cannot write in.
    pub fn send_typing(
        &mut self,
        protocol: ProtocolId,
        chat: &str,
        topic: Option<MessageTopic>,
        now: Instant,
    ) {
        if !self.is_selected_thread(protocol, chat, topic)
            || self.editing.is_some()
            || !self.can_type(protocol)
        {
            return;
        }
        let empty = self.compose.trim().is_empty();
        let thread = (protocol, chat.to_owned(), topic);
        if let Some(typing) = self.typing.draft_changed(thread, empty, now) {
            self.push(AdapterCommand::SetTyping {
                protocol,
                conversation_id: chat.to_owned(),
                topic,
                typing,
            });
        }
    }

    /// End the typing state of this account: the user turned it off.
    pub fn stop_typing(&mut self, now: Instant) {
        if let Some((protocol, conversation_id, topic)) = self.typing.stop(now) {
            self.push(AdapterCommand::SetTyping {
                protocol,
                conversation_id,
                topic,
                typing: false,
            });
        }
    }

    /// Drop the people with no typing event for some seconds. `true` when
    /// a name went away.
    pub fn expire_typing(&mut self, now: Instant) -> bool {
        self.typing.expire(now)
    }

    /// The open thread can get a typing state: the linked account of
    /// `protocol` has typing, and the chat and topic take messages.
    fn can_type(&self, protocol: ProtocolId) -> bool {
        let typing = self
            .accounts
            .iter()
            .any(|row| row.caps.id == protocol && row.linked() && row.caps.typing);
        let topic_open = self.selected_topic_row().is_none_or(|topic| !topic.closed);
        let retained = self
            .selected_conversation
            .as_deref()
            .is_some_and(|id| thinwire_protocol::signal_retained_slot(id).is_some());
        typing
            && topic_open
            && !retained
            && self
                .selected_conversation_row()
                .is_some_and(|row| row.writable)
    }

    /// Who types in the open thread, as one line: "Ada is typing…". `None`
    /// when nobody types.
    #[must_use]
    pub fn typing_line(&self) -> Option<String> {
        let chat = self.selected_conversation.clone()?;
        let thread = (self.selected_protocol, chat, self.selected_topic);
        crate::typing::typing_line(&self.typing.names(&thread))
    }

    /// Add photos and videos to the tray of one thread. Another file is
    /// skipped, and the tray says why. A file already in the tray is not
    /// added again. The tray holds [`MAX_ATTACHMENTS`] files at most.
    pub fn add_attachments(
        &mut self,
        protocol: ProtocolId,
        chat: &str,
        topic: Option<MessageTopic>,
        paths: Vec<std::path::PathBuf>,
    ) {
        if paths.is_empty() {
            return;
        }
        if !self.sends_media(protocol) {
            self.attachment_note = Some(ATTACH_NOT_HERE);
            return;
        }
        let key = (protocol, chat.to_owned(), topic);
        let tray = self.attachments.entry(key).or_default();
        let mut note = None;
        for path in paths {
            let Some(file) = OutgoingFile::new(path) else {
                note = Some(ATTACH_ONLY_MEDIA);
                continue;
            };
            if tray.contains(&file) {
                continue;
            }
            if tray.len() >= MAX_ATTACHMENTS {
                note = Some(ATTACH_TOO_MANY);
                break;
            }
            tray.push(file);
        }
        self.attachment_note = note;
    }

    /// Take one file out of the tray of a thread.
    pub fn remove_attachment(
        &mut self,
        protocol: ProtocolId,
        chat: &str,
        topic: Option<MessageTopic>,
        index: usize,
    ) {
        let key = (protocol, chat.to_owned(), topic);
        if let Some(tray) = self.attachments.get_mut(&key)
            && index < tray.len()
        {
            tray.remove(index);
            if tray.is_empty() {
                self.attachments.remove(&key);
            }
        }
        self.attachment_note = None;
    }

    /// The tray of the open thread.
    #[must_use]
    pub fn compose_attachments(&self) -> &[OutgoingFile] {
        let Some(chat) = self.selected_conversation.clone() else {
            return &[];
        };
        self.attachments
            .get(&(self.selected_protocol, chat, self.selected_topic))
            .map_or(&[], Vec::as_slice)
    }

    /// Why the last attach skipped files, if it did.
    #[must_use]
    pub const fn attachment_note(&self) -> Option<&'static str> {
        self.attachment_note
    }

    /// The linked account of `protocol` can send photos and videos.
    #[must_use]
    pub fn sends_media(&self, protocol: ProtocolId) -> bool {
        self.accounts
            .iter()
            .any(|row| row.caps.id == protocol && row.linked() && row.caps.sends_media)
    }

    /// An accepted send took these files: drop them from the tray, when the
    /// tray still holds them.
    fn clear_sent_files(
        &mut self,
        protocol: ProtocolId,
        chat: &str,
        topic: Option<MessageTopic>,
        files: &[OutgoingFile],
    ) {
        if files.is_empty() {
            return;
        }
        let key = (protocol, chat.to_owned(), topic);
        if let Some(tray) = self.attachments.get_mut(&key) {
            tray.retain(|file| !files.contains(file));
            if tray.is_empty() {
                self.attachments.remove(&key);
            }
        }
    }

    /// True when this chat and topic are the open thread.
    #[must_use]
    pub fn is_selected_thread(
        &self,
        protocol: ProtocolId,
        chat: &str,
        topic: Option<MessageTopic>,
    ) -> bool {
        self.is_selected_chat(protocol, chat) && self.selected_topic == topic
    }

    /// What the message menu offers for the linked account of `protocol`.
    #[must_use]
    pub fn message_actions(&self, protocol: ProtocolId) -> thinwire_protocol::MessageActions {
        self.accounts
            .iter()
            .find(|row| row.caps.id == protocol && row.linked())
            .map_or(thinwire_protocol::MessageActions::NONE, |row| {
                row.caps.message_actions
            })
    }

    /// The message that the next send of the open thread replies to.
    #[must_use]
    pub fn reply_target(&self) -> Option<&ChatMessage> {
        let target = self.replying.as_ref()?;
        if !self.is_selected_thread(target.protocol, &target.conversation_id, target.topic) {
            return None;
        }
        self.selected_messages()
            .iter()
            .find(|message| message.id == target.message_id)
    }

    /// The next send of the open thread replies to this message.
    pub(crate) fn begin_reply(
        &mut self,
        protocol: ProtocolId,
        conversation_id: &str,
        message_id: &str,
    ) {
        if !self.message_actions(protocol).reply
            || !self.is_selected_thread(protocol, conversation_id, self.selected_topic)
            || !self
                .selected_messages()
                .iter()
                .any(|row| row.id == message_id)
        {
            return;
        }
        self.cancel_edit();
        self.replying = Some(ReplyTarget {
            protocol,
            conversation_id: conversation_id.to_owned(),
            topic: self.selected_topic,
            message_id: message_id.to_owned(),
        });
    }

    /// The next send is a plain message again.
    pub(crate) fn cancel_reply(&mut self) {
        self.replying = None;
    }

    /// The reply id of a send in the open thread, when the user chose one.
    fn take_reply(&mut self) -> Option<String> {
        let id = self.reply_target().map(|message| message.id.clone());
        if id.is_some() {
            self.replying = None;
        }
        id
    }

    /// The own message that the compose field of the open thread edits.
    #[must_use]
    pub fn edit_target(&self) -> Option<&ChatMessage> {
        let target = self.editing.as_ref()?;
        if !self.is_selected_thread(target.protocol, &target.conversation_id, target.topic) {
            return None;
        }
        self.selected_messages()
            .iter()
            .find(|message| message.id == target.message_id)
    }

    /// Edit one own text message of the open thread: its text goes in the
    /// compose field, and the draft waits until the edit ends.
    pub(crate) fn begin_edit(
        &mut self,
        protocol: ProtocolId,
        conversation_id: &str,
        message_id: &str,
    ) {
        if !self.message_actions(protocol).edit
            || !self.is_selected_thread(protocol, conversation_id, self.selected_topic)
        {
            return;
        }
        let Some(body) = self
            .selected_messages()
            .iter()
            .find(|row| row.id == message_id)
            .filter(|row| can_edit(row))
            .map(|row| row.body.clone())
        else {
            return;
        };
        let draft = match self.editing.take() {
            Some(old) => old.draft,
            None => std::mem::take(&mut self.compose),
        };
        self.replying = None;
        self.editing = Some(EditTarget {
            protocol,
            conversation_id: conversation_id.to_owned(),
            topic: self.selected_topic,
            message_id: message_id.to_owned(),
            draft,
        });
        self.compose = body;
        self.focus_compose = true;
    }

    /// End the edit with no change. The draft of the thread comes back.
    pub(crate) fn cancel_edit(&mut self) {
        if let Some(target) = self.editing.take() {
            self.compose = target.draft;
        }
    }

    // region: forward

    /// The selected messages of the open thread, in the order of selection.
    /// Empty when the selection is of another thread.
    #[must_use]
    pub fn selected_for_forward(&self) -> &[String] {
        match &self.selection {
            Some(selection)
                if self.is_selected_thread(
                    selection.protocol,
                    &selection.conversation_id,
                    selection.topic,
                ) =>
            {
                &selection.message_ids
            }
            _ => &[],
        }
    }

    /// The open thread is in selection mode: a click on a message selects it.
    #[must_use]
    pub fn selecting(&self) -> bool {
        !self.selected_for_forward().is_empty()
    }

    /// A message of the open thread that can be forwarded on `protocol`.
    fn forwardable(&self, protocol: ProtocolId, conversation_id: &str, message_id: &str) -> bool {
        self.message_actions(protocol).forward
            && self.is_selected_thread(protocol, conversation_id, self.selected_topic)
            && self
                .selected_messages()
                .iter()
                .any(|row| row.id == message_id && can_forward(row))
    }

    /// Add or remove one message of the open thread in the selection. A
    /// selection of another thread ends.
    pub fn toggle_selection(
        &mut self,
        protocol: ProtocolId,
        conversation_id: &str,
        message_id: &str,
    ) {
        if !self.forwardable(protocol, conversation_id, message_id) {
            return;
        }
        let topic = self.selected_topic;
        let same_thread = self.selection.as_ref().is_some_and(|selection| {
            selection.protocol == protocol
                && selection.conversation_id == conversation_id
                && selection.topic == topic
        });
        if !same_thread {
            self.selection = Some(MessageSelection {
                protocol,
                conversation_id: conversation_id.to_owned(),
                topic,
                message_ids: Vec::new(),
            });
        }
        let Some(selection) = self.selection.as_mut() else {
            return;
        };
        if let Some(at) = selection.message_ids.iter().position(|id| id == message_id) {
            selection.message_ids.remove(at);
        } else {
            selection.message_ids.push(message_id.to_owned());
        }
        if selection.message_ids.is_empty() {
            self.selection = None;
        }
    }

    /// End the selection mode.
    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    /// Open the forward picker for one message, or for the selection of the
    /// open thread when `message_id` is `None`.
    pub fn open_forward(
        &mut self,
        protocol: ProtocolId,
        conversation_id: &str,
        message_id: Option<&str>,
    ) {
        let message_ids: Vec<String> = match message_id {
            Some(id) => vec![id.to_owned()],
            None => self.selected_for_forward().to_vec(),
        };
        if message_ids.is_empty()
            || !message_ids
                .iter()
                .all(|id| self.forwardable(protocol, conversation_id, id))
        {
            return;
        }
        self.forwarding = Some(ForwardDraft {
            protocol,
            conversation_id: conversation_id.to_owned(),
            message_ids,
            query: String::new(),
        });
    }

    /// The search text of the forward picker.
    pub fn set_forward_query(&mut self, query: String) {
        if let Some(draft) = self.forwarding.as_mut() {
            draft.query = query;
        }
    }

    /// Close the forward picker. The selection stays.
    pub fn cancel_forward(&mut self) {
        self.forwarding = None;
    }

    /// The forward picker, when it is open.
    #[must_use]
    pub fn forward_picker(&self) -> Option<ForwardPicker<'_>> {
        let draft = self.forwarding.as_ref()?;
        let query = draft.query.trim().to_lowercase();
        let targets = self
            .conversations
            .get(&draft.protocol)
            .into_iter()
            .flatten()
            .filter(|row| self.chat_in_open_account(&row.id))
            .filter(|row| {
                query.is_empty()
                    || row.title.to_lowercase().contains(&query)
                    || row.participant.to_lowercase().contains(&query)
            })
            .collect();
        Some(ForwardPicker {
            count: draft.message_ids.len(),
            query: &draft.query,
            targets,
        })
    }

    /// Forward the messages of the picker to `target`, a chat of the same
    /// account. The picker and the selection close.
    pub fn forward_to(&mut self, target: &str) {
        let Some(draft) = self.forwarding.as_ref() else {
            return;
        };
        let known = self
            .conversations
            .get(&draft.protocol)
            .into_iter()
            .flatten()
            .any(|row| row.id == target && self.chat_in_open_account(&row.id));
        if !known {
            return;
        }
        let Some(draft) = self.forwarding.take() else {
            return;
        };
        let count = draft.message_ids.len();
        self.selection = None;
        self.push(AdapterCommand::ForwardMessages {
            protocol: draft.protocol,
            from_conversation_id: draft.conversation_id,
            message_ids: draft.message_ids,
            to_conversation_id: target.to_owned(),
        });
        self.status_text = if count == 1 {
            "Forwarding the message…".into()
        } else {
            format!("Forwarding {count} messages…")
        };
    }

    // endregion: forward

    /// Enter while the compose field edits a message: ask the adapter to
    /// save the new text. An empty text keeps the edit open. The same text
    /// ends the edit with no command.
    fn save_edit(&mut self) {
        let body = self.compose.trim().to_owned();
        let Some(message) = self.edit_target() else {
            self.cancel_edit();
            return;
        };
        if body.is_empty() {
            return;
        }
        let unchanged = message.body.trim() == body;
        let command = AdapterCommand::EditMessage {
            protocol: message.protocol,
            conversation_id: message.conversation_id.clone(),
            message_id: message.id.clone(),
            body,
        };
        self.cancel_edit();
        if !unchanged {
            self.error = None;
            self.queue(command);
        }
    }

    /// Ask the adapter to delete one message of the open chat. `for_everyone`
    /// needs an own message and a protocol that allows it.
    pub(crate) fn delete_message(
        &mut self,
        protocol: ProtocolId,
        conversation_id: &str,
        message_id: &str,
        for_everyone: bool,
    ) {
        let actions = self.message_actions(protocol);
        let Some(message) = self.selected_messages().iter().find(|row| {
            row.protocol == protocol
                && row.conversation_id == conversation_id
                && row.id == message_id
        }) else {
            return;
        };
        let allowed = if for_everyone {
            actions.delete_for_everyone && message.outbound
        } else {
            actions.delete_for_me
        };
        if !allowed {
            return;
        }
        if self
            .replying
            .as_ref()
            .is_some_and(|target| target.message_id == message_id)
        {
            self.replying = None;
        }
        self.queue(AdapterCommand::DeleteMessage {
            protocol,
            conversation_id: conversation_id.to_owned(),
            message_id: message_id.to_owned(),
            for_everyone,
        });
    }

    /// Add or remove the reaction `emoji` of this account. The row shows the
    /// change at once. The next `MessageReactions` of the adapter replaces it.
    pub(crate) fn react(
        &mut self,
        protocol: ProtocolId,
        conversation_id: &str,
        message_id: &str,
        emoji: &str,
        add: bool,
    ) {
        let actions = self.message_actions(protocol);
        if !actions.react || emoji.is_empty() || emoji.len() > MAX_EMOJI_BYTES {
            return;
        }
        if !self.is_selected_thread(protocol, conversation_id, self.selected_topic) {
            return;
        }
        let Some(message) = self.message_mut(protocol, conversation_id, message_id) else {
            return;
        };
        thinwire_protocol::Reaction::toggle(
            &mut message.reactions,
            emoji,
            add,
            actions.one_reaction,
        );
        self.queue(AdapterCommand::React {
            protocol,
            conversation_id: conversation_id.to_owned(),
            message_id: message_id.to_owned(),
            emoji: emoji.to_owned(),
            add,
        });
    }

    fn set_selected_conversation(&mut self, id: Option<String>) {
        self.set_selection(id, None);
    }

    /// Change the open chat and topic. The compose text stays with the
    /// thread it was typed in.
    fn set_selection(&mut self, id: Option<String>, topic: Option<MessageTopic>) {
        if self.selected_conversation == id && self.selected_topic == topic {
            return;
        }
        // The edit text never becomes a draft.
        self.cancel_edit();
        self.reply_jump = None;
        self.scroll_to_message = None;
        let draft = std::mem::take(&mut self.compose);
        if let Some(old) = self.selected_conversation.clone()
            && !draft.is_empty()
        {
            self.drafts
                .insert((self.selected_protocol, old, self.selected_topic), draft);
        }
        self.selected_topic = topic;
        if let Some(new) = id.as_ref() {
            self.compose = self
                .drafts
                .remove(&(self.selected_protocol, new.clone(), topic))
                .unwrap_or_default();
        }
        self.selected_conversation = id;
    }

    /// Drop older-page memory of one chat, for every topic.
    fn forget_older_chat(&mut self, protocol: ProtocolId, chat: &str) {
        let same =
            |key: &(ProtocolId, String, Option<MessageTopic>)| key.0 == protocol && key.1 == chat;
        self.older_loading.retain(|key| !same(key));
        self.older_at_start.retain(|key| !same(key));
        self.older_retry.retain(|key, _| !same(key));
        self.older_note.retain(|key| !same(key));
        self.unreadable.retain(|key, _| !same(key));
        self.open_unreadable.retain(|key, _| !same(key));
        self.history_resume.retain(|key, _| !same(key));
        self.history_asked.retain(|key, _| !same(key));
    }

    /// Drop older-page memory of one protocol.
    fn forget_older_protocol(&mut self, protocol: ProtocolId) {
        self.older_loading.retain(|key| key.0 != protocol);
        self.older_at_start.retain(|key| key.0 != protocol);
        self.older_retry.retain(|key, _| key.0 != protocol);
        self.older_note.retain(|key| key.0 != protocol);
        self.unreadable.retain(|key, _| key.0 != protocol);
        self.open_unreadable.retain(|key, _| key.0 != protocol);
        self.history_resume.retain(|key, _| key.0 != protocol);
        self.history_asked.retain(|key, _| key.0 != protocol);
    }

    /// Every adapter answered `Shutdown` with `Stopped`: no session of any
    /// protocol (TDLib, the WhatsApp bot and its SQLite session) still runs.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn stopped_contains(&self, account: ProtocolId) -> bool {
        self.stopped.contains(&account)
    }

    pub fn all_stopped(&self) -> bool {
        self.accounts
            .iter()
            .all(|row| self.stopped.contains(&row.caps.id))
    }

    /// True once after the user picked a chat. The UI then focuses compose.
    /// A focus request waits for the compose field. Read-only; see `take_focus_compose`.
    #[must_use]
    pub const fn wants_focus_compose(&self) -> bool {
        self.focus_compose
    }

    pub fn take_focus_compose(&mut self) -> bool {
        std::mem::take(&mut self.focus_compose)
    }

    /// Send is possible (shell plan 5): the selected protocol is linked and
    /// can send text, the selected chat is writable, text exists, and the
    /// chat has no send or retry in flight.
    #[must_use]
    pub fn can_send(&self) -> bool {
        let protocol = self.selected_protocol;
        let sends_text = self
            .accounts
            .iter()
            .any(|row| row.caps.id == protocol && row.linked() && row.caps.sends_text);
        let topic_open = self.selected_topic_row().is_none_or(|topic| !topic.closed);
        // A previous Signal number stays in the switcher. It cannot send.
        let retained = self
            .selected_conversation
            .as_deref()
            .is_some_and(|id| thinwire_protocol::signal_retained_slot(id).is_some());
        if self.edit_target().is_some() {
            return sends_text && !retained && !self.compose.trim().is_empty();
        }
        let has_files = !self.compose_attachments().is_empty();
        sends_text
            && !retained
            && topic_open
            && (!self.compose.trim().is_empty() || has_files)
            && (!has_files || self.sends_media(protocol))
            && self
                .selected_conversation_row()
                .is_some_and(|row| row.writable && !self.sends.in_flight(protocol, &row.id))
    }

    /// Enter in compose. Plain Enter sends and returns `true`, so the UI eats
    /// the key. Shift+Enter returns `false`, so the text field adds a line.
    pub fn compose_enter(&mut self, shift: bool) -> bool {
        if shift {
            return false;
        }
        self.send_compose();
        true
    }

    /// Send a failed outgoing message again. Only a `Failed` row queues a command.
    pub fn retry_send(&mut self, message_id: &str) {
        let Some(conversation_id) = self.selected_conversation.clone() else {
            return;
        };
        let protocol = self.selected_protocol;
        // The same checks as Send (PR #68 review): the protocol is linked and
        // sends text, and the chat is writable. The tracker checks in-flight.
        let sends_text = self
            .accounts
            .iter()
            .any(|row| row.caps.id == protocol && row.linked() && row.caps.sends_text);
        let writable = self
            .selected_conversation_row()
            .is_some_and(|row| row.writable);
        let retained = thinwire_protocol::signal_retained_slot(&conversation_id).is_some();
        if !sends_text || !writable || retained {
            return;
        }
        let Some(message) = self
            .messages
            .get_mut(&thread_key(protocol, &conversation_id, self.selected_topic))
            .and_then(|list| list.iter_mut().find(|row| row.id == message_id))
        else {
            return;
        };
        if !message.outbound || message.delivery != Delivery::Failed {
            return;
        }
        // One send or retry per chat. Only this retry's own answer ends it.
        let Some(request) =
            self.sends
                .begin_retry(protocol, &conversation_id, message_id, Instant::now())
        else {
            return;
        };
        message.delivery = Delivery::Pending;
        if self.compose == message.body {
            self.compose.clear();
        }
        self.error = None;
        self.push(AdapterCommand::ResendMessage {
            protocol,
            conversation_id,
            message_id: message_id.to_string(),
            request,
        });
    }

    /// New inbox search. A highlight that the query hides is cleared.
    pub fn set_search(&mut self, text: String) {
        if self.search == text {
            return;
        }
        self.search = text;
        self.sync_focused_row(FocusFollow::Query);
    }

    /// Discord inbox chrome when feature `discord-bot` is compiled.
    ///
    /// Visibility follows that compile-time check alone. It does not wait for
    /// a Telegram session or for Telegram messages.
    #[must_use]
    pub fn discord_inbox_visible(&self) -> bool {
        DiscordAdapter::bot_inbox_compiled()
    }

    /// Protocols that may appear in Accounts / filter chrome for this build.
    ///
    /// Telegram is always present. Slack, WhatsApp, and Discord appear only
    /// when their cargo features are on.
    #[must_use]
    pub fn account_surface_visible(&self, protocol: ProtocolId) -> bool {
        #[cfg(any(test, feature = "test-support"))]
        if self.hidden_for_test.contains(&protocol) {
            return false;
        }
        if self.extra_visible.contains(&protocol) {
            return true;
        }
        if protocol.is(ProtocolKind::Discord) {
            self.discord_inbox_visible()
        } else {
            protocol_chrome_enabled(protocol)
        }
    }

    /// The account sidebar lists every account this build shows. Picking one
    /// does not hide the others. The middle column is still only the
    /// selected account ([`Self::visible_conversations`]).
    #[must_use]
    pub fn shows_in_switcher(&self, protocol: ProtocolId) -> bool {
        self.account_surface_visible(protocol)
    }

    /// Chats in the middle column: the selected account only. There is no
    /// combined inbox.
    pub fn visible_conversations(&self) -> Vec<&Conversation> {
        if !self.account_surface_visible(self.selected_protocol) {
            return Vec::new();
        }
        // Retained Signal history is on this computer. It stays when the
        // live session ends.
        let retained_open =
            self.selected_protocol == ProtocolId::Signal && self.selected_signal_retained.is_some();
        if !retained_open && !self.has_session(self.selected_protocol) {
            return Vec::new();
        }
        let query = self.search.trim().to_ascii_lowercase();
        self.conversations
            .get(&self.selected_protocol)
            .into_iter()
            .flatten()
            .filter(|row| self.chat_in_open_account(&row.id))
            .filter(|row| {
                query.is_empty()
                    || row.title.to_ascii_lowercase().contains(&query)
                    || row.participant.to_ascii_lowercase().contains(&query)
            })
            .collect()
    }

    #[must_use]
    pub fn unread_for(&self, protocol: ProtocolId) -> u32 {
        if !self.has_session(protocol) {
            return 0;
        }
        self.conversations
            .get(&protocol)
            .map(|rows| rows.iter().map(|row| row.unread).sum())
            .unwrap_or(0)
    }

    /// Unread on an account chip. A forum contributes unmuted topic counts,
    /// or its own unread until those topics are loaded.
    #[must_use]
    pub(crate) fn account_display_unread(&self, protocol: ProtocolId, mutes: &ChatMutes) -> u32 {
        if !self.has_session(protocol) {
            return 0;
        }
        self.conversations
            .get(&protocol)
            .map(|rows| {
                rows.iter()
                    .map(|row| self.display_unread(row, mutes))
                    .fold(0, u32::saturating_add)
            })
            .unwrap_or(0)
    }

    /// Unread on one account chip. A previous Signal number counts only its
    /// own chats. The live row does not count those chats.
    #[must_use]
    pub(crate) fn account_display_unread_for(
        &self,
        account: &AccountRow,
        mutes: &ChatMutes,
    ) -> u32 {
        if account.signal_retained.is_none() && !self.has_session(account.caps.id) {
            return 0;
        }
        self.conversations
            .get(&account.caps.id)
            .map(|rows| {
                rows.iter()
                    .filter(|row| {
                        !account.caps.id.is(ProtocolKind::Signal)
                            || thinwire_protocol::signal_retained_slot(&row.id)
                                == account.signal_retained
                    })
                    .map(|row| self.display_unread(row, mutes))
                    .fold(0, u32::saturating_add)
            })
            .unwrap_or(0)
    }

    /// The selected protocol pages its history (`pages_history`, ADR 0010
    /// rule 10) and its account is linked. Telegram also needs its TDLib
    /// session (Ready). A previous Signal number pages from `retained/`
    /// with no live session.
    fn pages_older(&self) -> bool {
        let protocol = self.selected_protocol;
        let pages = self
            .accounts
            .iter()
            .any(|row| row.caps.id == protocol && row.caps.pages_history);
        if protocol == ProtocolId::Signal && self.selected_signal_retained.is_some() {
            return pages;
        }
        match protocol {
            ProtocolId::Telegram => pages && self.telegram_authorized,
            _ => pages && self.protocol_linked(protocol),
        }
    }

    fn protocol_linked(&self, protocol: ProtocolId) -> bool {
        self.accounts
            .iter()
            .any(|row| row.caps.id == protocol && row.linked())
    }

    pub fn selected_conversation_row(&self) -> Option<&Conversation> {
        let id = self.selected_conversation.as_ref()?;
        self.conversations
            .get(&self.selected_protocol)?
            .iter()
            .find(|row| row.id == *id)
    }

    #[must_use]
    pub fn inbox_state(&self) -> InboxState {
        if !self.visible_conversations().is_empty() {
            return InboxState::Rows;
        }
        let has_rows = self.has_session(self.selected_protocol)
            && self
                .conversations
                .get(&self.selected_protocol)
                .is_some_and(|rows| !rows.is_empty());
        if has_rows {
            return InboxState::NoMatch;
        }
        if self.chat_list_loading.contains(&self.selected_protocol)
            && self.has_session(self.selected_protocol)
        {
            return InboxState::Loading;
        }
        InboxState::Empty
    }

    /// The line in an empty middle column.
    ///
    /// `Some(LOADING_CHATS_STATUS)` while an online account still syncs.
    /// `Some(NO_CHATS_YET)` only after a link, when that account has no chats.
    /// `None` before the account is linked, and when rows or a search fill
    /// the column.
    #[must_use]
    pub fn inbox_placeholder(&self) -> Option<&'static str> {
        match self.inbox_state() {
            InboxState::Loading => Some(LOADING_CHATS_STATUS),
            InboxState::Empty if self.protocol_linked(self.selected_protocol) => Some(NO_CHATS_YET),
            InboxState::Rows | InboxState::Empty | InboxState::NoMatch => None,
        }
    }

    /// Open the README risk text. A protocol with no unofficial notice is ignored.
    /// The link screens and the worker commands stay as they are.
    pub fn open_notice_review(&mut self, protocol: ProtocolId) {
        if unofficial_notice(protocol).is_none() {
            return;
        }
        self.notice_review = Some(protocol);
    }

    pub fn close_notice_review(&mut self) {
        self.notice_review = None;
    }

    /// The protocol whose README risk text is open, if any.
    #[must_use]
    pub const fn notice_review(&self) -> Option<ProtocolId> {
        self.notice_review
    }

    /// The status line is a finished success: the adapter sent it with
    /// `AdapterStatus::Ready`, nothing replaced it since, and no load runs.
    /// The kind comes from the adapter status and the load state, not from
    /// the text (#56).
    #[must_use]
    pub fn status_is_idle(&self) -> bool {
        !self.is_loading() && self.ready_status.as_deref() == Some(self.status_text.as_str())
    }

    /// A chat list, a history page, an older page, or a send is still running. A `Ready`
    /// line such as "Loading recent messages." is not idle then (#64 review).
    #[must_use]
    pub fn is_loading(&self) -> bool {
        !self.chat_list_loading.is_empty()
            || !self.history_loading.is_empty()
            || !self.older_loading.is_empty()
            || !self.sends.is_empty()
            || !self.telegram_send_unconfirmed.is_empty()
    }

    /// The line of a running load of one protocol, if one runs (#80).
    #[must_use]
    pub fn loading_line(&self, protocol: ProtocolId) -> Option<&'static str> {
        if self.chat_list_loading.contains(&protocol) {
            Some(LOADING_CHATS_STATUS)
        } else if self
            .history_loading
            .iter()
            .any(|(owner, _)| *owner == protocol)
        {
            Some(LOADING_MESSAGES_STATUS)
        } else if self.older_loading.iter().any(|key| key.0 == protocol) {
            // F1 (#82): older pages of this protocol only, keyed by
            // (protocol, chat, topic).
            Some(LOADING_OLDER_STATUS)
        } else if self.sends.compose_in_flight(protocol)
            || (protocol.is(ProtocolKind::Telegram)
                && self
                    .telegram_send_unconfirmed
                    .iter()
                    .any(|hold| !hold.retry))
        {
            // Telegram's accept is not the end of a compose send. The strip
            // stays on Sending… until that row is sent or failed. A retry
            // already shows "Sending…" on its row, so the strip stays quiet.
            Some(SENDING_STATUS)
        } else {
            None
        }
    }

    /// The line the status strip shows (#80). The selected protocol's own
    /// load shows first. A Telegram failure that is still the last status
    /// then wins over a load of another protocol (#90). Otherwise another
    /// visible protocol's load is named. With no load, it is the last status
    /// line.
    #[must_use]
    pub fn status_line(&self) -> std::borrow::Cow<'_, str> {
        self.status_source().0
    }

    /// The status line is the line of a running load. The strip shows it in
    /// the busy colour. A finished line or a failure is not busy (#82 qa L1).
    #[must_use]
    pub fn status_line_loads(&self) -> bool {
        self.status_source().1
    }

    /// The status line, and whether it is a running load's line. The selected
    /// protocol's own load shows first, including an older page while a
    /// Telegram failure is still the last status (#73). That failure then
    /// wins over a load of another protocol (#90).
    fn status_source(&self) -> (std::borrow::Cow<'_, str>, bool) {
        if let Some(line) = self.loading_line(self.selected_protocol) {
            return (line.into(), true);
        }
        if self.failure_status.as_deref() == Some(self.status_text.as_str()) {
            return (self.status_text.as_str().into(), false);
        }
        let other = self
            .accounts
            .iter()
            .map(|row| row.caps.id)
            .filter(|id| *id != self.selected_protocol && self.account_surface_visible(*id))
            .find_map(|id| self.loading_line(id).map(|line| (id, line)));
        match other {
            Some((id, line)) => (format!("{}: {line}", id.display_name()).into(), true),
            None => (self.status_text.as_str().into(), false),
        }
    }

    #[must_use]
    pub fn thread_state(&self) -> ThreadState {
        let Some(id) = self.selected_conversation.as_ref() else {
            return ThreadState::NoSelection;
        };
        if !self.selected_messages().is_empty() {
            return ThreadState::Rows;
        }
        if self
            .history_loading
            .contains(&(self.selected_protocol, id.clone()))
        {
            return ThreadState::Loading;
        }
        ThreadState::Empty
    }

    /// Why the last older page of the selected thread did not load (#57).
    #[must_use]
    pub fn older_note(&self) -> Option<&'static str> {
        let id = self.selected_conversation.clone()?;
        self.older_note
            .contains(&older_key(self.selected_protocol, &id, self.selected_topic))
            .then_some(OLDER_PAGE_FAILED_NOTE)
    }

    /// Older-message paging of the selected chat (#30).
    #[must_use]
    pub fn older_state(&self) -> OlderState {
        let Some(id) = self.selected_conversation.as_ref() else {
            return OlderState::Idle;
        };
        if !self.pages_older() {
            return OlderState::Idle;
        }
        let key = older_key(self.selected_protocol, id, self.selected_topic);
        if self.older_loading.contains(&key) {
            OlderState::Loading
        } else if self.older_at_start.contains(&key) {
            OlderState::StartOfChat
        } else {
            OlderState::Idle
        }
    }

    /// Ask for the page before the oldest loaded message of the selected
    /// chat, for a protocol with `pages_history`. Nothing goes out
    /// while a request runs, the first page loads, or the start is loaded.
    pub(crate) fn load_older(&mut self) {
        self.load_older_at(Instant::now());
    }

    /// `load_older` with the clock as a parameter, for tests. An anchor that
    /// brought nothing older waits `OLDER_RETRY_DELAY` from `now`.
    fn load_older_at(&mut self, now: Instant) {
        // A retained chat pages through the helper. The helper reads that
        // store and does not send.
        let Some((id, oldest)) = self.older_anchor_at(now) else {
            return;
        };
        let protocol = self.selected_protocol;
        let topic = self.selected_topic;
        let store_cursor = self.take_resume_cursor(protocol, &id, topic);
        self.older_loading.insert(older_key(protocol, &id, topic));
        if let Some(topic) = topic {
            self.push(AdapterCommand::LoadOlderTopic {
                protocol,
                conversation_id: id,
                topic,
                before_message_id: oldest,
            });
        } else {
            self.push(AdapterCommand::LoadOlderMessages {
                protocol,
                conversation_id: id,
                before_message_id: oldest,
                store_cursor,
            });
        }
    }

    /// Closed runs of unreadable rows in the open thread.
    #[must_use]
    pub fn unreadable_runs(&self) -> &[UnreadableRun] {
        let Some(id) = self.selected_conversation.as_ref() else {
            return &[];
        };
        self.unreadable
            .get(&older_key(self.selected_protocol, id, self.selected_topic))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    fn oldest_message_id(
        &self,
        protocol: ProtocolId,
        chat: &str,
        topic: Option<MessageTopic>,
    ) -> Option<String> {
        self.messages
            .get(&thread_key(protocol, chat, topic))
            .and_then(|list| list.first())
            .map(|row| row.id.clone())
    }

    /// Record one page's store cursor. `true` when the next page was queued
    /// at once, because this page held no rows.
    fn note_unreadable(&mut self, page: UnreadablePage<'_>) -> bool {
        let UnreadablePage {
            protocol,
            chat,
            topic,
            request_before,
            cursor,
            more,
            initial,
        } = page;
        let key = older_key(protocol, chat, topic);
        let oldest = self.oldest_message_id(protocol, chat, topic);
        let added = if initial {
            oldest.is_some()
        } else {
            oldest.as_deref().is_some_and(|id| id != request_before)
        };
        let moves = self
            .history_asked
            .get(&key)
            .is_none_or(|asked| cursor.before_ms < *asked);
        if !added && cursor.resume && moves && more && topic.is_none() {
            let before_id = self.open_unreadable.get(&key).map_or_else(
                || {
                    if initial || request_before.is_empty() {
                        None
                    } else {
                        Some(request_before.to_string())
                    }
                },
                |open| open.before_id.clone(),
            );
            let skipped = self
                .open_unreadable
                .get(&key)
                .map(|open| open.skipped)
                .unwrap_or(0)
                .saturating_add(cursor.skipped);
            self.open_unreadable
                .insert(key, OpenUnreadable { before_id, skipped });
            return self.queue_store_page(protocol, chat, request_before, cursor.before_ms);
        }
        let pending = self.open_unreadable.remove(&key);
        if cursor.runs.is_empty() {
            let skipped = pending
                .as_ref()
                .map(|open| open.skipped)
                .unwrap_or(0)
                .saturating_add(cursor.skipped);
            if skipped > 0 {
                let before_id = pending.map_or_else(
                    || Self::unreadable_anchor(initial, request_before, oldest),
                    |open| open.before_id,
                );
                self.unreadable
                    .entry(key.clone())
                    .or_default()
                    .push(UnreadableRun { before_id, skipped });
            }
        } else {
            let mut leading = pending.as_ref().map(|open| open.skipped).unwrap_or(0);
            let pending_anchor = pending.map(|open| open.before_id);
            let mut edge = 0u32;
            let mut placed = Vec::new();
            for run in cursor.runs {
                if run.skipped == 0 {
                    continue;
                }
                match run.after_id {
                    None => leading = leading.saturating_add(run.skipped),
                    Some(id) if self.message_is_loaded(protocol, chat, topic, &id) => {
                        push_unreadable(&mut placed, Some(id), run.skipped);
                    }
                    Some(_) => {
                        tracing::warn!(
                            "history run names a message that is not loaded; the count joins the oldest line"
                        );
                        edge = edge.saturating_add(run.skipped);
                    }
                }
            }
            if edge > 0 {
                push_unreadable(&mut placed, oldest.clone(), edge);
            }
            if leading > 0 {
                let before_id = pending_anchor
                    .unwrap_or_else(|| Self::unreadable_anchor(initial, request_before, oldest));
                push_unreadable(&mut placed, before_id, leading);
            }
            if !placed.is_empty() {
                self.unreadable
                    .entry(key.clone())
                    .or_default()
                    .extend(placed);
            }
        }
        if cursor.resume && moves && more && added && topic.is_none() {
            self.history_resume.insert(key, cursor.before_ms);
        } else {
            self.history_resume.remove(&key);
        }
        false
    }

    fn message_is_loaded(
        &self,
        protocol: ProtocolId,
        chat: &str,
        topic: Option<MessageTopic>,
        id: &str,
    ) -> bool {
        self.messages
            .get(&thread_key(protocol, chat, topic))
            .is_some_and(|rows| rows.iter().any(|row| row.id == id))
    }

    fn unreadable_anchor(
        initial: bool,
        request_before: &str,
        oldest: Option<String>,
    ) -> Option<String> {
        if initial {
            oldest
        } else if request_before.is_empty() {
            None
        } else {
            Some(request_before.to_string())
        }
    }

    fn close_open_run(&mut self, key: &OlderPageKey) {
        let Some(open) = self.open_unreadable.remove(key) else {
            return;
        };
        if open.skipped == 0 {
            return;
        }
        self.unreadable
            .entry(key.clone())
            .or_default()
            .push(UnreadableRun {
                before_id: open.before_id,
                skipped: open.skipped,
            });
    }

    /// Queue the next page from `before_ms`. `false` when that timestamp does
    /// not move older than the last request.
    fn queue_store_page(
        &mut self,
        protocol: ProtocolId,
        chat: &str,
        request_before: &str,
        before_ms: u64,
    ) -> bool {
        let key = older_key(protocol, chat, None);
        if self
            .history_asked
            .get(&key)
            .is_some_and(|asked| before_ms >= *asked)
        {
            self.history_resume.remove(&key);
            return false;
        }
        self.history_resume.insert(key.clone(), before_ms);
        self.history_asked.insert(key.clone(), before_ms);
        self.older_loading.insert(key.clone());
        self.older_retry.remove(&key);
        self.older_at_start.remove(&key);
        let before_message_id = if request_before.is_empty() {
            before_ms.to_string()
        } else {
            request_before.to_string()
        };
        self.pending.push(AdapterCommand::LoadOlderMessages {
            protocol,
            conversation_id: chat.to_string(),
            before_message_id,
            store_cursor: Some(before_ms),
        });
        true
    }

    /// The saved store cursor, when it is older than the last finished
    /// request. A failed read clears that timestamp, so the same cursor is
    /// sent again. A cursor that a finished read did not move is dropped.
    fn take_resume_cursor(
        &mut self,
        protocol: ProtocolId,
        chat: &str,
        topic: Option<MessageTopic>,
    ) -> Option<u64> {
        if topic.is_some() {
            return None;
        }
        let key = older_key(protocol, chat, topic);
        let cursor = self.history_resume.get(&key).copied()?;
        if self
            .history_asked
            .get(&key)
            .is_some_and(|asked| cursor >= *asked)
        {
            self.history_resume.remove(&key);
            return None;
        }
        self.history_asked.insert(key, cursor);
        Some(cursor)
    }

    /// A request for older messages of the selected chat would go out now.
    /// A frontend checks it before it sends the intent, so a thread that
    /// cannot scroll asks again after the wait, and not on every repaint
    /// (#67).
    #[must_use]
    pub fn older_can_ask(&self) -> bool {
        self.older_anchor_at(Instant::now()).is_some()
    }

    /// The selected chat and its oldest message, when an older request may
    /// go out at `now`.
    fn older_anchor_at(&self, now: Instant) -> Option<(String, String)> {
        if !self.pages_older() {
            return None;
        }
        let protocol = self.selected_protocol;
        let id = self.selected_conversation.clone()?;
        let key = older_key(protocol, &id, self.selected_topic);
        let history_key = (protocol, id.clone());
        if self.history_loading.contains(&history_key)
            || self.older_loading.contains(&key)
            || self.older_at_start.contains(&key)
        {
            return None;
        }
        let oldest = self.selected_messages().first()?.id.clone();
        if let Some((anchor, at, tries)) = self.older_retry.get(&key)
            && *anchor == oldest
            && now.saturating_duration_since(*at) < older_wait(*tries)
        {
            return None;
        }
        Some((id, oldest))
    }

    /// True once after the user opens a chat. The inbox scrolls that row into view.
    /// A scroll request waits for the inbox rows. Read-only; see `take_scroll_to_selected`.
    #[must_use]
    pub const fn wants_scroll_to_selected(&self) -> bool {
        self.scroll_to_selected
    }

    pub fn take_scroll_to_selected(&mut self) -> bool {
        std::mem::take(&mut self.scroll_to_selected)
    }

    /// Scroll the open thread to the message that a reply answers. When that
    /// message is not loaded, older pages load first, at most
    /// [`REPLY_JUMP_PAGES`]. A thread with no older pages does nothing.
    pub fn jump_to_message(&mut self, message_id: &str) {
        self.reply_jump = None;
        if self
            .selected_messages()
            .iter()
            .any(|row| row.id == message_id)
        {
            self.scroll_to_message = Some(message_id.to_owned());
            return;
        }
        let Some(chat) = self.selected_conversation.as_deref() else {
            return;
        };
        if !self.pages_older() {
            return;
        }
        self.reply_jump = Some(ReplyJump {
            thread: older_key(self.selected_protocol, chat, self.selected_topic),
            message_id: message_id.to_owned(),
            pages_left: REPLY_JUMP_PAGES,
        });
        self.continue_reply_jump();
    }

    /// Scroll to the waiting reply target when it is loaded. Otherwise load
    /// the next older page, while pages are left and a page may go out.
    fn continue_reply_jump(&mut self) {
        let Some(mut jump) = self.reply_jump.take() else {
            return;
        };
        let Some(chat) = self.selected_conversation.as_deref() else {
            return;
        };
        if jump.thread != older_key(self.selected_protocol, chat, self.selected_topic) {
            return;
        }
        if self
            .selected_messages()
            .iter()
            .any(|row| row.id == jump.message_id)
        {
            self.scroll_to_message = Some(jump.message_id);
            return;
        }
        if self.older_loading.contains(&jump.thread) {
            // A page is on its way. Its answer continues the jump.
            self.reply_jump = Some(jump);
            return;
        }
        if jump.pages_left == 0 || self.older_anchor_at(Instant::now()).is_none() {
            return;
        }
        jump.pages_left -= 1;
        self.reply_jump = Some(jump);
        self.load_older();
    }

    /// The message that the thread scrolls into view once. Read-only; see
    /// `take_scroll_to_message`.
    #[must_use]
    pub fn wants_scroll_to_message(&self) -> Option<&str> {
        self.scroll_to_message.as_deref()
    }

    pub fn take_scroll_to_message(&mut self) -> Option<String> {
        self.scroll_to_message.take()
    }

    /// True once after the keyboard highlight moves. The inbox scrolls that row into view.
    #[must_use]
    pub const fn wants_scroll_to_focused(&self) -> bool {
        self.scroll_to_focused
    }

    pub fn take_scroll_to_focused(&mut self) -> bool {
        std::mem::take(&mut self.scroll_to_focused)
    }

    pub fn selected_messages(&self) -> &[ChatMessage] {
        let Some(id) = self.selected_conversation.as_ref() else {
            return &[];
        };
        self.messages
            .get(&thread_key(self.selected_protocol, id, self.selected_topic))
            .map_or(&[], Vec::as_slice)
    }

    /// The open topic, when one is selected.
    #[must_use]
    pub fn selected_topic_row(&self) -> Option<&ChatTopic> {
        let id = self.selected_conversation.as_ref()?;
        let topic = self.selected_topic?;
        self.topics
            .get(&(self.selected_protocol, id.clone()))
            .and_then(|rows| rows.iter().find(|row| row.topic == topic))
    }

    /// Topics of the selected chat, in list order.
    #[must_use]
    pub fn selected_topics(&self) -> Vec<&ChatTopic> {
        let Some(id) = self.selected_conversation.as_ref() else {
            return Vec::new();
        };
        self.topics
            .get(&(self.selected_protocol, id.clone()))
            .map(|topics| ordered_topics(topics))
            .unwrap_or_default()
    }

    /// One topic of one chat.
    #[must_use]
    pub fn topic(
        &self,
        protocol: ProtocolId,
        conversation_id: &str,
        topic: MessageTopic,
    ) -> Option<&ChatTopic> {
        self.topics
            .get(&(protocol, conversation_id.to_owned()))
            .and_then(|rows| rows.iter().find(|row| row.topic == topic))
    }

    /// The badge of one row. A forum or a bot chat sums the unread counts
    /// of the topics that are not muted. Until those topics are loaded, the
    /// row's own unread is the badge.
    #[must_use]
    pub(crate) fn display_unread(&self, row: &Conversation, mutes: &ChatMutes) -> u32 {
        if !row.has_topics {
            return row.unread;
        }
        let Some(topics) = self.topics.get(&(row.protocol, row.id.clone())) else {
            return row.unread;
        };
        if topics.is_empty() {
            return row.unread;
        }
        topics
            .iter()
            .filter(|topic| {
                !ChatMute::of(
                    topic.muted,
                    mutes.contains(row.protocol, &topic_mute_id(&row.id, topic.topic)),
                )
                .is_muted()
            })
            .map(|topic| topic.unread)
            .fold(0, u32::saturating_add)
    }

    pub fn refresh_visible(&mut self) {
        let protocols: HashSet<ProtocolId> = self
            .accounts
            .iter()
            .map(|row| row.caps.id)
            .filter(|id| self.account_surface_visible(*id))
            .collect();
        for protocol in protocols {
            if self.protocol_linked(protocol) {
                self.chat_list_loading.insert(protocol);
                self.push(AdapterCommand::LoadChats { protocol });
            } else if !protocol.is(ProtocolKind::Slack) {
                // Slack Connect with no token opens the browser. Only the
                // Add Slack workspace button starts that install.
                self.push(AdapterCommand::Connect { protocol });
            }
        }
        self.status_text = "Refreshing…".into();
    }

    #[must_use]
    pub fn has_api_credentials(&self, store: &SecretStore) -> bool {
        telegram_api_available(store, &self.api_source)
    }

    #[must_use]
    pub fn telegram_ready(&self) -> bool {
        self.telegram_authorized
    }

    /// A new login of Telegram account 0 is offered only while it is not
    /// signed in (ux F8). A second account has its own login (ADR 0014).
    #[must_use]
    pub fn can_add_account(&self) -> bool {
        !self.telegram_authorized
    }

    pub fn open_add_account(&mut self, store: &SecretStore) {
        if !self.can_add_account() {
            return;
        }
        self.open_telegram(store);
    }

    pub fn cancel_auth(&mut self, store: &SecretStore) {
        self.clear_secrets();
        self.resume = Resume::Settled;
        clear_ephemeral(store);
        self.auth = AuthScreen::Idle;
        self.auth_busy = false;
        self.error = None;
        if self.telegram_authorized {
            // A form over a live session (for example Advanced): close the
            // form only. The session and the chat list stay (ux F8).
            self.status_text = "Telegram is ready.".into();
            return;
        }
        self.status_text = "Account linking cancelled.".into();
        self.push(AdapterCommand::Disconnect {
            protocol: self.telegram_login_account,
        });
        self.restore_telegram_login();
    }

    pub fn advance_telegram(&mut self, store: &SecretStore) {
        if self.auth_busy {
            return;
        }
        match self.auth {
            AuthScreen::TelegramApi => {
                let api_id = self.telegram_api_id.clone();
                let api_hash = self.telegram_api_hash.clone();
                if !self.require_field("api_id", &api_id) {
                    return;
                }
                if !self.require_field("api_hash", &api_hash) {
                    return;
                }
                if let Err(error) = persist_api(store, &api_id, &api_hash) {
                    self.set_error(
                        "Telegram credentials were not stored.",
                        &error.to_string(),
                        "Cancel and try again. Values are not logged.",
                    );
                    return;
                }
                self.keychain_flush = true;
                self.queue_telegram_step(TelegramAuthStep::ApiCredentials);
                self.mark_auth_busy(
                    "Telegram: api credentials stored. Waiting for the next login step.",
                );
            }
            AuthScreen::TelegramConnecting => {
                // Try again after a failed start: close the old client, then
                // start a new one. The new one waits for the old one to close.
                self.push(AdapterCommand::Disconnect {
                    protocol: self.telegram_login_account,
                });
                self.queue_telegram_step(TelegramAuthStep::ApiCredentials);
                self.mark_auth_busy(CONNECTING_STATUS);
            }
            AuthScreen::TelegramPhone => {
                let phone = self.telegram_phone.clone();
                if !self.require_field("phone number", &phone) {
                    return;
                }
                store.set_secret(SecretKey::Phone, &phone);
                self.queue_telegram_step(TelegramAuthStep::Phone);
                self.mark_auth_busy("Telegram: phone stored. Waiting for a login code.");
            }
            AuthScreen::TelegramCode => {
                let code = self.telegram_code.clone();
                if !self.require_field("login code", &code) {
                    return;
                }
                store.set_secret(SecretKey::Code, &code);
                self.queue_telegram_step(TelegramAuthStep::Code);
                self.mark_auth_busy("Telegram: login code stored. Waiting for the next step.");
            }
            AuthScreen::Telegram2fa => {
                // TDLib asks for a password only when the account has one.
                if self.telegram_2fa.is_empty() {
                    return;
                }
                store.set_secret(SecretKey::Password, &self.telegram_2fa);
                self.queue_telegram_step(TelegramAuthStep::TwoFactor);
                self.mark_auth_busy("Telegram: password sent. Waiting for Telegram.");
            }
            AuthScreen::NeedCredentials | AuthScreen::Idle => {}
        }
    }

    /// Enter runs the main button of the center screen. Escape cancels a login.
    /// Compose handles its own Enter, so the thread view ignores keys here.
    pub fn center_key(&mut self, key: AuthKey, store: &SecretStore) {
        match self.center_view() {
            CenterView::FirstRun => {
                // Same call as the Add Telegram button, with its guard (qa L3).
                if key == AuthKey::Enter {
                    self.open_add_account(store);
                }
            }
            CenterView::Auth => self.auth_key(key, store),
            CenterView::KeychainFailed => {
                if key == AuthKey::Enter {
                    self.retry_keychain();
                }
            }
            CenterView::Resuming { .. } => {}
            CenterView::Thread => {
                if key == AuthKey::Escape {
                    self.close_open_topic();
                }
            }
        }
    }

    /// Try again after a failed keychain read. The app starts the read.
    pub fn retry_keychain(&mut self) {
        self.keychain_retry = true;
        self.keychain_wait_started = None;
        self.error = None;
    }

    /// True once after Try again: read the keychain again off the UI thread.
    pub fn take_keychain_retry(&mut self) -> bool {
        std::mem::take(&mut self.keychain_retry)
    }

    /// The adapter accepted this send (chat and request id match). Only now
    /// does the compose text (or the chat's draft) clear. A history message
    /// with the same text is not an acceptance (Codex 4091552898).
    fn note_send_accepted(&mut self, protocol: ProtocolId, chat: &str, request: u64) {
        match self.sends.settle(protocol, chat, request) {
            // An accepted retry needs nothing more: its delivery events move the row.
            Some(Pending::Send {
                body, topic, files, ..
            }) => {
                self.drop_rejected_body(protocol, chat);
                self.clear_sent_text(protocol, chat, topic, &body);
                self.clear_sent_files(protocol, chat, topic, &files);
                self.hold_telegram_send_line(protocol, chat, request, false);
            }
            Some(Pending::Retry { .. }) => {
                self.drop_rejected_body(protocol, chat);
                self.hold_telegram_send_line(protocol, chat, request, true);
            }
            None => self.note_late_accept(protocol, chat, request),
        }
    }

    /// A `SendAccepted` after the send expired (PR #81 review). The message
    /// went out, so the user must not send it again: clear the compose text
    /// only if it still holds exactly that text, mark a retried row sent, and
    /// drop the "did not answer in time" error.
    fn note_late_accept(&mut self, protocol: ProtocolId, chat: &str, request: u64) {
        let Some(pending) = self.sends.settle_expired(protocol, chat, request) else {
            return;
        };
        let retry = matches!(pending, Pending::Retry { .. });
        self.drop_rejected_body(protocol, chat);
        match pending {
            Pending::Send {
                body, topic, files, ..
            } => {
                self.clear_sent_text(protocol, chat, topic, &body);
                self.clear_sent_files(protocol, chat, topic, &files);
            }
            Pending::Retry { message_id, .. } => {
                self.set_delivery(protocol, chat, &message_id, Delivery::Sent);
                // A new retry of the same row must not fail it again.
                self.sends.drop_retry_of(protocol, chat, &message_id);
            }
        }
        // Only this send's part of the timeout error goes.
        let shown = self.error.is_some() && self.error == self.timeout_error;
        self.timed_out.retain(|entry| {
            !(entry.protocol == protocol && entry.chat == chat && entry.request == request)
        });
        if shown {
            self.show_timeouts();
        }
        self.hold_telegram_send_line(protocol, chat, request, retry);
    }

    /// Set the timeout error from `timed_out`: one line per expired send, or
    /// no error when the list is empty.
    fn show_timeouts(&mut self) {
        let error = match self.timed_out.as_slice() {
            [] => None,
            [one] => Some(UserError {
                happened: "Message not sent.".into(),
                why: format!("{} did not answer in time.", one.protocol.display_name()),
                next: self.resend_hint(one.protocol, &one.chat, one.topic, one.retry),
            }),
            many => {
                let chats: Vec<String> = many
                    .iter()
                    .map(|entry| {
                        format!(
                            "{} ({})",
                            self.chat_title(entry.protocol, &entry.chat),
                            entry.protocol.display_name()
                        )
                    })
                    .collect();
                Some(UserError {
                    happened: format!("{} messages not sent.", many.len()),
                    why: format!("No answer in time for {}.", chats.join(", ")),
                    next: "Each text is still in its chat. Send it again, or press Retry.".into(),
                })
            }
        };
        self.error.clone_from(&error);
        self.timeout_error = error;
    }

    /// Build the shown timeout error again, so its hint names where the
    /// text is now (#90 item 6): the compose field of the chat that shows,
    /// or a draft after a chat switch. The core calls it after every
    /// dispatch and pump. Another error, or no error, stays as it is.
    pub(crate) fn sync_timeout_hint(&mut self) {
        if self.error.is_some() && self.error == self.timeout_error && !self.timed_out.is_empty() {
            self.show_timeouts();
        }
    }

    /// Where the unsent text is, and what to do (qa on #81): the compose
    /// field of the chat that shows, or the draft of another chat.
    fn resend_hint(
        &self,
        protocol: ProtocolId,
        chat: &str,
        topic: Option<MessageTopic>,
        retry: bool,
    ) -> String {
        let selected = self.is_selected_thread(protocol, chat, topic);
        match (retry, selected) {
            (false, true) => "The text is still in the compose field. Send it again.".into(),
            (false, false) => format!(
                "The text is in the draft of {}. Send it again.",
                self.chat_title(protocol, chat)
            ),
            (true, true) => "Press Retry to send it again.".into(),
            (true, false) => format!(
                "Open {} and press Retry to send it again.",
                self.chat_title(protocol, chat)
            ),
        }
    }

    /// The title of a chat, or its id when the row is gone.
    fn chat_title(&self, protocol: ProtocolId, chat: &str) -> String {
        self.conversations
            .get(&protocol)
            .and_then(|rows| rows.iter().find(|row| row.id == chat))
            .map_or_else(|| chat.to_owned(), |row| row.title.clone())
    }

    /// The text of an accepted send left the compose field or its draft, if
    /// it is still exactly that text. Only the topic the send was typed in.
    fn clear_sent_text(
        &mut self,
        protocol: ProtocolId,
        chat: &str,
        topic: Option<MessageTopic>,
        body: &str,
    ) {
        if self.is_selected_thread(protocol, chat, topic) && self.compose.trim() == body {
            self.compose.clear();
        }
        let key = (protocol, chat.to_owned(), topic);
        if self
            .drafts
            .get(&key)
            .is_some_and(|draft| draft.trim() == body)
        {
            self.drafts.remove(&key);
        }
    }

    /// Expire sends and retries with no answer after `SEND_TIMEOUT` (#69).
    /// The core calls it on every pump.
    /// Returns true when an entry expired, so the state changed.
    pub(crate) fn expire_sends(&mut self) -> bool {
        self.expire_sends_at(Instant::now())
    }

    /// Forget timed-out sends older than `EXPIRED_KEEP` (#90 item 3). Call it
    /// after the queued events are applied.
    pub(crate) fn prune_late_answers(&mut self) {
        self.sends.prune_expired(Instant::now());
        self.prune_unbound_telegram_messages();
    }

    /// When the next send or retry expires, if one is in flight.
    /// An accepted Telegram send is still in flight until its own end, so its
    /// deadline is here too. The core arms one wake for the earliest (#81).
    pub(crate) fn next_send_deadline(&self) -> Option<Instant> {
        let accepted = self
            .telegram_send_unconfirmed
            .iter()
            .map(|hold| hold.at + crate::sends::SEND_TIMEOUT)
            .min();
        match (self.sends.next_deadline(), accepted) {
            (Some(open), Some(held)) => Some(open.min(held)),
            (open, held) => open.or(held),
        }
    }

    /// Every send that has no final answer yet, oldest first: sends and
    /// retries the adapter has not answered, then Telegram sends that TDLib
    /// took but the server has not confirmed. For the log at close (#238).
    pub(crate) fn unfinished_sends(&self) -> Vec<UnfinishedSend> {
        let mut out: Vec<UnfinishedSend> = self
            .sends
            .open_chats()
            .into_iter()
            .map(|(protocol, chat)| UnfinishedSend {
                protocol,
                chat,
                stage: SendStage::NoAnswer,
            })
            .collect();
        out.extend(
            self.telegram_send_unconfirmed
                .iter()
                .map(|hold| UnfinishedSend {
                    protocol: ProtocolId::Telegram,
                    chat: hold.chat.clone(),
                    stage: SendStage::NotConfirmed,
                }),
        );
        out
    }

    /// Test hook: a saved Telegram session resumes now (`Resume::Connecting`).
    #[cfg(test)]
    pub(crate) fn resume_connecting_for_test(&mut self) {
        self.resume = Resume::Connecting;
    }

    /// Test hook: age every tracked send by `by`.
    #[cfg(test)]
    pub(crate) fn age_sends_for_test(&mut self, by: Duration) {
        self.sends.age_for_test(by);
        for hold in &mut self.telegram_send_unconfirmed {
            hold.at -= by;
        }
    }

    /// `expire_sends` with the clock as a parameter, for tests. An expired
    /// send keeps its text; an expired retry sets its row back to `Failed`.
    fn expire_sends_at(&mut self, now: Instant) -> bool {
        let before = self.telegram_send_unconfirmed.len();
        self.end_telegram_send(ProtocolId::Telegram, TelegramSendEnd::Expired(now));
        let telegram_ended = self.telegram_send_unconfirmed.len() != before;
        let expired = self.sends.expire(now);
        if expired.is_empty() {
            self.prune_unbound_telegram_messages();
            return telegram_ended;
        }
        if self.error != self.timeout_error {
            self.timed_out.clear();
        }
        for (protocol, chat, pending) in expired {
            let (retry, topic) = match &pending {
                Pending::Send { topic, .. } => (false, *topic),
                Pending::Retry { message_id, .. } => {
                    self.set_delivery(protocol, &chat, message_id, Delivery::Failed);
                    let topic = self
                        .message_bucket(protocol, &chat, message_id)
                        .map(|key| key.2)
                        .unwrap_or(None);
                    (true, topic)
                }
            };
            self.timed_out.push(TimedOut {
                protocol,
                chat,
                request: pending.request(),
                retry,
                topic,
            });
        }
        // Every expired send shows, not only the last one (qa on #81).
        self.show_timeouts();
        self.prune_unbound_telegram_messages();
        true
    }

    /// The adapter rejected this send (chat and request id match): it was
    /// not accepted. The text is still in its compose field or draft. Other
    /// errors (for example a history load error) never fail a send.
    fn fail_unaccepted_send(&mut self, protocol: ProtocolId, chat: &str, request: u64) {
        self.end_telegram_send(protocol, TelegramSendEnd::Request(request));
        let why = format!("{} did not accept the message.", protocol.display_name());
        let settled = self.sends.settle(protocol, chat, request);
        if settled.is_none() {
            // A late rejection after the send expired changes nothing: the
            // expiry already failed it (PR #81 review).
            let _ = self.sends.settle_expired(protocol, chat, request);
        }
        match settled {
            Some(Pending::Send { body, topic, .. }) => {
                if !body.is_empty() {
                    let rejected = self
                        .rejected_bodies
                        .entry((protocol, chat.to_owned()))
                        .or_default();
                    rejected.compose = Some(body);
                    rejected.topic = topic;
                }
                let next = self.resend_hint(protocol, chat, topic, false);
                self.set_error("Message not sent.", &why, &next);
            }
            Some(Pending::Retry { message_id, .. }) => {
                self.set_delivery(protocol, chat, &message_id, Delivery::Failed);
                let topic = self
                    .message_bucket(protocol, chat, &message_id)
                    .map(|key| key.2)
                    .unwrap_or(None);
                if let Some(text) = self
                    .message_body(protocol, chat, &message_id)
                    .filter(|text| !text.is_empty())
                {
                    let rejected = self
                        .rejected_bodies
                        .entry((protocol, chat.to_owned()))
                        .or_default();
                    rejected.row = Some(text);
                    rejected.topic = topic;
                }
                let next = self.resend_hint(protocol, chat, topic, true);
                self.set_error("Message not sent.", &why, &next);
            }
            None => {}
        }
        self.prune_unbound_telegram_messages();
    }

    /// Forget a rejected send for this chat. An accepted send, a removed
    /// chat, or a session end that no longer shows the text all use this.
    fn drop_rejected_body(&mut self, protocol: ProtocolId, chat: &str) {
        self.rejected_bodies.remove(&(protocol, chat.to_owned()));
    }

    /// Compose-rejected text wins when it still matches. Otherwise a retry's
    /// text fills an empty draft.
    /// The unsent text of a chat now: the compose field of the chat that
    /// shows, or the chat's draft.
    fn current_text(&self, protocol: ProtocolId, chat: &str, topic: Option<MessageTopic>) -> &str {
        if self.is_selected_thread(protocol, chat, topic) {
            self.compose.trim()
        } else {
            self.drafts
                .get(&(protocol, chat.to_owned(), topic))
                .map_or("", |draft| draft.trim())
        }
    }

    /// The text of each send or retry with no answer that a session end
    /// keeps as its chat's draft (#135): one in flight, or one that expired
    /// and still waits for a late answer (Codex r4109058384). The chat's text
    /// must be empty or still that text. A retry keeps the text of its row,
    /// which the session end drops. A send in flight comes last, so it wins
    /// over an older expired send of the same chat.
    fn pending_texts_to_keep(
        &self,
        protocol: ProtocolId,
    ) -> Vec<(String, Option<MessageTopic>, String)> {
        self.sends
            .expired_of(protocol)
            .into_iter()
            .chain(self.sends.open_of(protocol))
            .filter_map(|(chat, pending)| {
                let (topic, text) = match pending {
                    Pending::Send { body, topic, .. } => (topic, body),
                    Pending::Retry { message_id, .. } => {
                        let topic = self
                            .message_bucket(protocol, &chat, &message_id)
                            .map(|key| key.2)
                            .unwrap_or(None);
                        (topic, self.message_body(protocol, &chat, &message_id)?)
                    }
                };
                let current = self.current_text(protocol, &chat, topic);
                (!text.is_empty() && (current.is_empty() || current == text))
                    .then_some((chat, topic, text))
            })
            .collect()
    }

    /// Rejected text and text still in flight. Collected before a Signal
    /// session end removes the live chats: that removal drops the rejected
    /// body, so a later read would find nothing (#135, #384).
    fn texts_to_keep_on_session_end(
        &self,
        protocol: ProtocolId,
    ) -> Vec<(String, Option<MessageTopic>, String)> {
        if protocol.is(ProtocolKind::Telegram) {
            return Vec::new();
        }
        let mut keep: Vec<(String, Option<MessageTopic>, String)> = self
            .rejected_bodies
            .iter()
            .filter_map(|((owner, chat), rejected)| {
                if *owner != protocol {
                    return None;
                }
                self.text_to_keep(protocol, chat, rejected)
                    .map(|text| (chat.clone(), rejected.topic, text))
            })
            .collect();
        // A send in flight is newer than an earlier rejection of its chat.
        keep.extend(self.pending_texts_to_keep(protocol));
        keep
    }

    fn text_to_keep(
        &self,
        protocol: ProtocolId,
        chat: &str,
        rejected: &RejectedBody,
    ) -> Option<String> {
        let current = self.current_text(protocol, chat, rejected.topic);
        if let Some(compose) = &rejected.compose
            && current == compose
        {
            return Some(compose.clone());
        }
        rejected
            .row
            .as_ref()
            .filter(|row| {
                current.is_empty() || (rejected.compose.is_none() && current == row.as_str())
            })
            .cloned()
    }

    fn message_body(&self, protocol: ProtocolId, chat: &str, message_id: &str) -> Option<String> {
        let key = self.message_bucket(protocol, chat, message_id)?;
        self.messages
            .get(&key)
            .and_then(|rows| rows.iter().find(|row| row.id == message_id))
            .map(|row| row.body.trim().to_owned())
    }

    /// Enter submits the current login step. Escape cancels the login.
    pub fn auth_key(&mut self, key: AuthKey, store: &SecretStore) {
        match (key, self.auth) {
            (_, AuthScreen::Idle) => {}
            (AuthKey::Escape, _) => self.cancel_auth(store),
            (AuthKey::Enter, AuthScreen::NeedCredentials) => self.open_api_override(store),
            (AuthKey::Enter, _) => self.advance_telegram(store),
        }
    }

    /// The submit button is enabled. The 2FA step needs a password.
    #[must_use]
    pub fn can_submit_auth(&self) -> bool {
        !self.auth_busy && !(self.auth == AuthScreen::Telegram2fa && self.telegram_2fa.is_empty())
    }

    /// Back from the code step to the phone step. No command here: TDLib
    /// 1.8.61 accepts `setAuthenticationPhoneNumber` in `authorizationStateWaitCode`
    /// (when no auth query is pending), so the next phone submit moves the live
    /// TDLib state to the new number and sends a new code.
    pub fn change_number(&mut self) {
        if self.auth != AuthScreen::TelegramCode {
            return;
        }
        self.auth = AuthScreen::TelegramPhone;
        self.auth_busy = false;
        self.telegram_code.clear();
        self.code_via = None;
        self.auth_rejection = None;
        self.error = None;
        self.status_text = "Telegram: enter a phone number.".into();
    }

    /// Ask Telegram for a new code with TDLib `resendAuthenticationCode`
    /// (the `ResendCode` step). The code step stays on screen.
    pub fn resend_code(&mut self) {
        if self.auth != AuthScreen::TelegramCode || self.auth_busy {
            return;
        }
        self.telegram_code.clear();
        self.queue_telegram_step(TelegramAuthStep::ResendCode);
        self.mark_auth_busy("Telegram: asking for a new code.");
    }

    /// Queue the compose text. Does nothing when [`Self::can_send`] is false;
    /// the Send button is disabled in that case, so no error block shows.
    pub fn send_compose(&mut self) {
        if self.editing.is_some() {
            if self.can_send() {
                self.save_edit();
            }
            return;
        }
        if !self.can_send() {
            return;
        }
        let Some(conversation_id) = self.selected_conversation.clone() else {
            return;
        };
        let body = self.compose.trim().to_string();
        // Keep the text until the adapter accepts the send; see note_send_accepted.
        let protocol = self.selected_protocol;
        let files = self.compose_attachments().to_vec();
        let Some(request) = self.sends.begin_send_files(
            protocol,
            &conversation_id,
            &body,
            self.selected_topic,
            files.clone(),
            Instant::now(),
        ) else {
            return;
        };
        self.error = None;
        self.attachment_note = None;
        self.typing
            .message_sent(&(protocol, conversation_id.clone(), self.selected_topic));
        if !files.is_empty() {
            // The files stay in the tray until the adapter accepts them.
            self.push(AdapterCommand::SendMedia {
                protocol,
                conversation_id,
                topic: self.selected_topic,
                files,
                caption: body,
                request,
            });
        } else if let Some(topic) = self.selected_topic {
            let reply_to = self.take_reply();
            self.push(AdapterCommand::SendTopicText {
                protocol,
                conversation_id,
                topic,
                body,
                request,
                reply_to,
            });
        } else {
            let reply_to = self.take_reply();
            self.push(AdapterCommand::SendText {
                protocol,
                conversation_id,
                body,
                request,
                reply_to,
            });
        }
    }

    /// Telegram's accept is not its finished line. Hold that request until it ends.
    fn hold_telegram_send_line(
        &mut self,
        protocol: ProtocolId,
        chat: &str,
        request: u64,
        retry: bool,
    ) {
        if !protocol.is(ProtocolKind::Telegram) {
            return;
        }
        let message_id = self
            .telegram_unbound_messages
            .iter()
            .position(|row| row.request == request && row.chat == chat)
            .map(|index| self.telegram_unbound_messages.remove(index).message_id);
        self.telegram_send_unconfirmed.push(TelegramHold {
            request,
            chat: chat.to_owned(),
            at: Instant::now(),
            message_id,
            retry,
        });
    }

    /// Point the hold or queued id that still names `old_id` at `new_id`.
    fn rebind_telegram_message(&mut self, old_id: &str, new_id: &str) -> bool {
        if let Some(hold) = self
            .telegram_send_unconfirmed
            .iter_mut()
            .find(|hold| hold.message_id.as_deref() == Some(old_id))
        {
            hold.message_id = Some(new_id.to_owned());
            return true;
        }
        if let Some(row) = self
            .telegram_unbound_messages
            .iter_mut()
            .find(|row| row.message_id == old_id)
        {
            row.message_id = new_id.to_owned();
            return true;
        }
        false
    }

    /// Drop a queued id when the send tracker no longer keeps its request
    /// (#168 item 10). The tracker keeps an expired request for
    /// `EXPIRED_KEEP` from the pump that expired it, so the id lives exactly
    /// as long as a late accept still counts, also after a late pump (for
    /// example after a suspend). A newer request never receives it.
    fn prune_unbound_telegram_messages(&mut self) {
        self.telegram_unbound_messages
            .retain(|row| self.sends.tracks(ProtocolId::Telegram, row.request));
    }

    /// Remember an outbound pending row for its own send (#168 item 9).
    /// While a send of this chat is in flight, the row is that send's: it
    /// waits for that request's `SendAccepted`. The newest row wins, because
    /// the adapter writes the row of a send just before its accept. With no
    /// send in flight, the row binds to an accepted send of this chat that
    /// has no row yet, but only when exactly one such send exists.
    fn note_telegram_pending(&mut self, message: &ChatMessage) {
        let chat = &message.conversation_id;
        if let Some(request) = self.sends.request_of(message.protocol, chat) {
            if let Some(row) = self
                .telegram_unbound_messages
                .iter_mut()
                .find(|row| row.request == request)
            {
                row.message_id.clone_from(&message.id);
                return;
            }
            self.telegram_unbound_messages.push(TelegramQueuedId {
                request,
                chat: chat.clone(),
                message_id: message.id.clone(),
            });
            return;
        }
        let mut unbound = self
            .telegram_send_unconfirmed
            .iter_mut()
            .filter(|hold| hold.chat == *chat && hold.message_id.is_none());
        if let (Some(hold), None) = (unbound.next(), unbound.next()) {
            hold.message_id = Some(message.id.clone());
        }
    }

    /// Release the hold bound to this message id: its row is now sent or failed.
    fn release_telegram_for_message(&mut self, message_id: &str) {
        let Some(request) = self
            .telegram_send_unconfirmed
            .iter()
            .find(|hold| hold.message_id.as_deref() == Some(message_id))
            .map(|hold| hold.request)
        else {
            return;
        };
        self.end_telegram_send(ProtocolId::Telegram, TelegramSendEnd::Request(request));
    }

    /// Bind a pending Telegram row to its send, or release that send when the
    /// row is sent or failed. `replaced` is the pending id a new id took over.
    fn observe_telegram_message(&mut self, message: &ChatMessage, replaced: Option<&str>) {
        if !message.protocol.is(ProtocolKind::Telegram) || !message.outbound {
            return;
        }
        match message.delivery {
            Delivery::Pending => {
                let rebound = replaced
                    .is_some_and(|old_id| self.rebind_telegram_message(old_id, &message.id));
                if !rebound {
                    // A retry emits the new pending row as a replacement of the
                    // failed id, before `SendAccepted`. Queue that new id.
                    self.note_telegram_pending(message);
                }
            }
            Delivery::Sent | Delivery::Read | Delivery::Failed => {
                if let Some(old_id) = replaced {
                    self.release_telegram_for_message(old_id);
                }
                self.release_telegram_for_message(&message.id);
            }
        }
    }

    /// One place that ends a Telegram send hold: success, rejection, a failed
    /// delivery, the 30 s timeout, or the end of the session. A release names
    /// the request. It does not drop a different send.
    fn end_telegram_send(&mut self, protocol: ProtocolId, end: TelegramSendEnd) {
        if !protocol.is(ProtocolKind::Telegram) {
            return;
        }
        match end {
            TelegramSendEnd::Request(request) => {
                self.telegram_send_unconfirmed
                    .retain(|hold| hold.request != request);
            }
            TelegramSendEnd::Expired(now) => {
                self.telegram_send_unconfirmed.retain(|hold| {
                    now.saturating_duration_since(hold.at) < crate::sends::SEND_TIMEOUT
                });
            }
            TelegramSendEnd::All => {
                self.telegram_send_unconfirmed.clear();
                self.telegram_unbound_messages.clear();
            }
        }
    }

    pub fn open_telegram(&mut self, store: &SecretStore) {
        // Until the keychain read ends, a saved DB key looks missing, and the
        // worker would move a good data folder aside.
        if store.read_failed() {
            self.set_error(
                "Telegram sign-in did not start.",
                "The keychain could not be read, so thinwire does not know your saved sign-in.",
                "Unlock the keychain, then press Try again.",
            );
            return;
        }
        if !store.attach_settled() {
            self.status_text = KEYCHAIN_LOADING_STATUS.into();
            return;
        }
        self.clear_secrets();
        self.resume = Resume::Settled;
        clear_ephemeral(store);
        self.error = None;
        self.auth_busy = false;
        if self.has_api_credentials(store) {
            self.start_phone_login();
            return;
        }
        self.auth = AuthScreen::NeedCredentials;
        self.status_text = if self.api_source.has_publisher() {
            "Telegram API credentials are missing from the keychain override. Set Advanced credentials or Cancel.".into()
        } else {
            "Credentials missing. Official binaries inject TELEGRAM_API_ID / TELEGRAM_API_HASH at release time. Dev: rebuild with those env vars, or set a keychain override in Advanced.".into()
        };
    }

    /// The API override applies to a new client only, so it is offered only
    /// while Telegram is not signed in (qa note on F8).
    pub fn open_api_override(&mut self, store: &SecretStore) {
        if !self.can_add_account() {
            return;
        }
        self.clear_secrets();
        self.error = None;
        self.auth_busy = false;
        self.prefill_from_store(store);
        self.auth = AuthScreen::TelegramApi;
        self.status_text =
            "Advanced: custom Telegram API credentials. The keychain override wins over the publisher pair. Values are not logged.".into();
    }

    fn start_phone_login(&mut self) {
        self.auth = AuthScreen::TelegramConnecting;
        self.queue_telegram_step(TelegramAuthStep::ApiCredentials);
        self.mark_auth_busy(CONNECTING_STATUS);
    }

    fn apply_telegram_phase(&mut self, phase: TelegramAuthPhase) {
        self.auth_busy = false;
        if phase != TelegramAuthPhase::Failed {
            self.error = None;
            self.auth_rejection = None;
        }
        let resuming = std::mem::replace(&mut self.resume, Resume::Settled) == Resume::Connecting;
        let unmarked = std::mem::take(&mut self.resume_unmarked) && resuming;
        self.auth_notice = None;
        if phase == TelegramAuthPhase::NeedPhone {
            // TDLib is back at the phone step: an old code or password is stale.
            self.telegram_code.clear();
            self.telegram_2fa.clear();
        }
        let login_step = matches!(
            phase,
            TelegramAuthPhase::NeedPhone
                | TelegramAuthPhase::NeedCode
                | TelegramAuthPhase::NeedTwoFactor
        );
        if unmarked && self.data_reset.is_none() && login_step {
            // The database holds no session, only a login that did not end.
            // Close it with no screen and no "session ended" notice: the
            // user did not start a login now.
            self.status_text = SIGN_IN_PROMPT.into();
            self.push(AdapterCommand::Disconnect {
                protocol: self.telegram_login_account,
            });
            return;
        }
        match phase {
            TelegramAuthPhase::NeedPhone if self.data_reset.is_some() => {
                let notice = data_reset_notice(&self.data_reset.take().unwrap_or_default());
                self.auth = AuthScreen::TelegramPhone;
                self.status_text.clone_from(&notice);
                self.auth_notice = Some(notice);
                // The saved session is gone with the old data folder: the
                // next login can be another account (#201 review).
                self.ended_accounts.push(self.telegram_login_account);
            }
            TelegramAuthPhase::NeedPhone if resuming => {
                // The worker drops the stale session marker on this path.
                self.auth = AuthScreen::TelegramPhone;
                self.auth_notice = Some(SESSION_ENDED_NOTICE.into());
                self.status_text = SESSION_ENDED_NOTICE.into();
                // The saved session ended while the app was closed: drop its
                // mutes, as a live session end does (#201 review).
                self.ended_accounts.push(self.telegram_login_account);
            }
            TelegramAuthPhase::NeedPhone => {
                self.auth = AuthScreen::TelegramPhone;
                self.status_text =
                    "Telegram: enter a phone number. It stays in the secret store.".into();
            }
            TelegramAuthPhase::NeedCode => {
                self.auth = AuthScreen::TelegramCode;
                self.status_text =
                    "Telegram: enter the login code. It stays in the secret store.".into();
            }
            TelegramAuthPhase::NeedTwoFactor => {
                self.auth = AuthScreen::Telegram2fa;
                self.status_text = "Telegram: enter your Telegram password.".into();
            }
            TelegramAuthPhase::Ready => self.finish_telegram_ready(),
            TelegramAuthPhase::Unavailable => self.finish_telegram_unavailable(),
            TelegramAuthPhase::Failed => {
                self.error = Some(auth_user_error(self.auth_rejection));
                if matches!(
                    self.auth_rejection,
                    Some(TelegramAuthError::ClientSetup { .. })
                ) {
                    // No step can run on this client. Leave the form, close the
                    // client, and let Add Telegram start a fresh one.
                    self.auth = AuthScreen::Idle;
                    self.clear_secrets();
                    self.push(AdapterCommand::Disconnect {
                        protocol: self.telegram_login_account,
                    });
                }
            }
        }
    }

    fn finish_telegram_ready(&mut self) {
        self.clear_secrets();
        if let Some(row) = self
            .accounts
            .iter_mut()
            .find(|row| row.caps.id == self.telegram_login_account)
        {
            row.state = AccountState::Linked;
        }
        let account = self.telegram_login_account;
        self.sessions.insert(account);
        self.telegram_authorized = true;
        // The worker loads the main list right after Ready.
        self.chat_list_loading.insert(account);
        // Keep a selection the user made in another linked protocol (#70).
        let keep = !self.selected_protocol.is(ProtocolKind::Telegram)
            && self.has_session(self.selected_protocol);
        if !keep {
            self.select_protocol(account);
        }
        self.auth = AuthScreen::Idle;
        self.auth_busy = false;
        self.error = None;
        self.status_text = "Telegram is ready. Loading the chat list.".into();
        // Another account linked: the screens are for account 0 again.
        self.restore_telegram_login();
    }

    /// The live session ended elsewhere. Drop the old inbox, then start a new
    /// client: the next phone step says the session ended (qa R73).
    fn end_telegram_session(&mut self) {
        if !self.telegram_authorized {
            return;
        }
        self.telegram_authorized = false;
        self.end_session(self.telegram_login_account);
        // The next account on this machine must not get this account's
        // mutes, as it does not get its drafts (#153 review).
        self.ended_accounts.push(self.telegram_login_account);
        self.auth = AuthScreen::Idle;
        self.auth_busy = false;
        self.error = None;
        // Reuse the resume path: spinner, then the phone step with the notice.
        self.resume = Resume::Connecting;
        self.queue_telegram_step(TelegramAuthStep::ApiCredentials);
        self.status_text = SESSION_ENDED_NOTICE.into();
    }

    fn finish_telegram_unavailable(&mut self) {
        self.clear_secrets();
        if let Some(row) = self
            .accounts
            .iter_mut()
            .find(|row| row.caps.id == self.telegram_login_account)
        {
            row.state = AccountState::Unlinked;
        }
        self.sessions.remove(&self.telegram_login_account);
        self.auth = AuthScreen::Idle;
        self.auth_busy = false;
        self.telegram_authorized = false;
        self.error = None;
        self.status_text =
            "TDLib unavailable in this build. No live Telegram session was opened; form fields were discarded."
                .into();
    }

    fn prefill_from_store(&mut self, store: &SecretStore) {
        if let Ok(Some(api_id)) = store.get(SecretKey::ApiId) {
            self.telegram_api_id = api_id;
        }
        if let Ok(Some(api_hash)) = store.get(SecretKey::ApiHash) {
            self.telegram_api_hash = api_hash;
        }
    }

    fn queue_telegram_step(&mut self, step: TelegramAuthStep) {
        // The host replaces `epoch` with the login client this step belongs to.
        self.push(AdapterCommand::TelegramAuth { step, epoch: 0 });
    }

    fn mark_auth_busy(&mut self, status: &str) {
        self.auth_busy = true;
        self.auth_rejection = None;
        self.error = None;
        self.status_text = status.into();
    }

    fn require_field(&mut self, name: &str, value: &str) -> bool {
        if value.trim().is_empty() {
            self.set_error(
                "Telegram login did not advance.",
                &format!("The {name} field is empty."),
                "Fill the field, or press Cancel. Values are not logged.",
            );
            return false;
        }
        true
    }

    fn clear_secrets(&mut self) {
        self.auth_notice = None;
        self.auth_rejection = None;
        self.code_via = None;
        self.telegram_api_id.clear();
        self.telegram_api_hash.clear();
        self.telegram_phone.clear();
        self.telegram_code.clear();
        self.telegram_2fa.clear();
    }

    fn set_error(&mut self, happened: &str, why: &str, next: &str) {
        self.error = Some(UserError {
            happened: happened.into(),
            why: why.into(),
            next: next.into(),
        });
    }

    fn ensure_conversation_selection(&mut self) {
        if self.selected_conversation.is_some() {
            return;
        }
        // A placeholder row is not a chat: never auto-select it (#70).
        // A previous Signal number is not a chat of the live account.
        let protocol = self.selected_protocol;
        let retained = self.selected_signal_retained;
        let first = self.conversations.get(&protocol).and_then(|rows| {
            rows.iter()
                .find(|row| {
                    !row.placeholder
                        && (!protocol.is(ProtocolKind::Signal)
                            || thinwire_protocol::signal_retained_slot(&row.id) == retained)
                })
                .map(|row| row.id.clone())
        });
        if let Some(id) = first {
            self.set_selected_conversation(Some(id));
            self.queue_open_chat();
        }
    }

    /// Load the selected chat's history, for any linked protocol (shell
    /// plan 3). The adapter checks its own id format; the core only needs the
    /// row in the protocol's list.
    fn queue_open_chat(&mut self) {
        let protocol = self.selected_protocol;
        let Some(id) = self.selected_conversation.clone() else {
            return;
        };
        // The previous number's messages are already loaded from its store.
        if thinwire_protocol::signal_retained_slot(&id).is_some() {
            return;
        }
        if !self.protocol_linked(protocol) {
            // Load it when the protocol links (#70).
            self.open_on_link = Some((protocol, id));
            return;
        }
        self.open_on_link = None;
        // Only a real chat loads history: never a placeholder row (#70).
        let listed = self
            .conversations
            .get(&protocol)
            .is_some_and(|rows| rows.iter().any(|row| row.id == id && !row.placeholder));
        if !listed {
            return;
        }
        let topic = self.selected_topic;
        let already = self.pending.iter().any(|command| match (command, topic) {
            (
                AdapterCommand::OpenTopic {
                    protocol: owner,
                    conversation_id,
                    topic: open,
                },
                Some(topic),
            ) => *owner == protocol && conversation_id == &id && *open == topic,
            (
                AdapterCommand::OpenChat {
                    protocol: owner,
                    conversation_id,
                },
                None,
            ) => *owner == protocol && conversation_id == &id,
            _ => false,
        });
        if already {
            return;
        }
        self.history_loading.insert((protocol, id.clone()));
        if let Some(topic) = topic {
            self.push(AdapterCommand::OpenTopic {
                protocol,
                conversation_id: id,
                topic,
            });
        } else {
            self.push(AdapterCommand::OpenChat {
                protocol,
                conversation_id: id,
            });
        }
    }

    /// Tell the adapters which chat shows now (shell plan 9). Sends only the
    /// difference: `None` to the protocol the user left, then the new chat.
    /// An unfocused window counts as no chat, so blur sends `None` and focus
    /// sends the open chat again. The core calls it after every dispatch and
    /// pump, so every path (a click, an auto-select, a removed chat, a
    /// session end, a login form over the thread, a focus change) is covered.
    pub(crate) fn sync_viewed(&mut self) {
        let now = self.viewed_chat();
        if now == self.viewed {
            return;
        }
        let before = std::mem::replace(&mut self.viewed, now.clone());
        let left_protocol = match (&before, &now) {
            (Some((old, _, _)), Some((new, _, _))) => (old != new).then_some(*old),
            (Some((old, _, _)), None) => Some(*old),
            (None, _) => None,
        };
        if let Some(protocol) = left_protocol {
            self.push(AdapterCommand::ViewChat {
                protocol,
                conversation_id: None,
            });
        }
        match now {
            Some((protocol, id, Some(topic))) => {
                self.push(AdapterCommand::ViewTopic {
                    protocol,
                    conversation_id: id,
                    topic,
                });
            }
            Some((protocol, id, None)) => {
                self.push(AdapterCommand::ViewChat {
                    protocol,
                    conversation_id: Some(id),
                });
            }
            None => {}
        }
        self.clear_viewed_unread();
    }

    /// The window gained or lost focus. Focus clears the open chat's badge.
    /// Blur changes [`Self::viewed_chat`], and the following [`Self::sync_viewed`]
    /// tells adapters the chat is not viewed. Focus sends that chat again.
    pub(crate) fn set_window_focus(&mut self, focused: bool) {
        self.window_focused = focused;
        if focused {
            self.clear_viewed_unread();
        }
    }

    fn chat_is_viewed(&self, protocol: ProtocolId, id: &str) -> bool {
        self.viewed
            .as_ref()
            .is_some_and(|(owner, chat, _topic)| *owner == protocol && chat == id)
    }

    /// The open chat is read once the window is focused. `ViewChat` is the
    /// signal that it is open. An unfocused window keeps the badge.
    fn clear_viewed_unread(&mut self) {
        if !self.window_focused {
            return;
        }
        let Some((protocol, id, topic)) = self.viewed.clone() else {
            return;
        };
        if let Some(topic) = topic {
            if let Some(rows) = self.topics.get_mut(&(protocol, id))
                && let Some(row) = rows.iter_mut().find(|row| row.topic == topic)
            {
                row.unread = 0;
            }
            return;
        }
        let has_topics = self
            .conversation(protocol, &id)
            .is_some_and(|row| row.has_topics);
        if has_topics {
            return;
        }
        let Some(list) = self.conversations.get_mut(&protocol) else {
            return;
        };
        if let Some(row) = list.iter_mut().find(|row| row.id == id) {
            row.unread = 0;
        }
    }

    /// The chat the user looks at, as last synced by `sync_viewed`.
    #[must_use]
    pub fn viewed(&self) -> Option<(ProtocolId, &str)> {
        self.viewed
            .as_ref()
            .map(|(protocol, id, _)| (*protocol, id.as_str()))
    }

    /// The open topic last synced to the adapters.
    #[must_use]
    pub fn viewed_topic(&self) -> Option<MessageTopic> {
        self.viewed.as_ref().and_then(|(_, _, topic)| *topic)
    }

    /// One chat row of one protocol.
    #[must_use]
    pub fn conversation(&self, protocol: ProtocolId, id: &str) -> Option<&Conversation> {
        self.conversations
            .get(&protocol)?
            .iter()
            .find(|row| row.id == id)
    }

    /// Unread messages of every chat that is not muted (in the protocol or
    /// in thinwire, #153), in every protocol with a session. For the window
    /// title and a taskbar badge (#32). A frontend calls
    /// [`crate::View::unread_total`].
    #[must_use]
    pub(crate) fn unread_total(&self, mutes: &ChatMutes) -> u32 {
        self.conversations
            .iter()
            .filter(|(protocol, _)| self.has_session(**protocol))
            .flat_map(|(_, rows)| rows.iter())
            .filter(|row| {
                !ChatMute::of(row.muted, mutes.contains(row.protocol, &row.id)).is_muted()
            })
            .map(|row| self.display_unread(row, mutes))
            .fold(0, u32::saturating_add)
    }

    /// The chat that shows in the thread now, if any.
    /// An unfocused window is not looking at it. Adapters that treat the
    /// viewed chat as read must hear `ViewChat(None)` or a live message
    /// while the user is away loses its badge.
    fn viewed_chat(&self) -> Option<(ProtocolId, String, Option<MessageTopic>)> {
        if !self.window_focused || self.center_view() != CenterView::Thread {
            return None;
        }
        let protocol = self.selected_protocol;
        let id = self.selected_conversation.clone()?;
        let row = self.conversation(protocol, &id)?;
        let listed = self.has_session(protocol)
            && self.account_surface_visible(protocol)
            && !row.placeholder;
        if !listed {
            return None;
        }
        if row.has_topics {
            let topic = self.selected_topic?;
            return Some((protocol, id, Some(topic)));
        }
        Some((protocol, id, None))
    }

    /// New link state of one protocol (`AdapterEvent::Account`).
    /// Rules (ADR 0010): `Linked` starts a session. `Linking` after `Linked`
    /// is a reconnect: the session, its rows, and its open sends stay, and
    /// send answers still apply; new commands wait for `Linked`. `Unlinked`
    /// from any other state ends the session.
    fn set_account(&mut self, protocol: ProtocolId, state: AccountState) {
        let had_session = self.has_session(protocol);
        let first_link = state == AccountState::Linked && !self.any_linked();
        let mut before = AccountState::Unlinked;
        if let Some(row) = self.live_account_mut(protocol) {
            before = std::mem::replace(&mut row.state, state);
        }
        match state {
            AccountState::Linked => {
                self.sessions.insert(protocol);
                // The first linked account, or the linked one while nothing
                // useful is selected: show it (shell plan 2).
                if first_link || !self.protocol_linked(self.selected_protocol) {
                    self.select_protocol(protocol);
                }
                // A chat opened while the account was Linking did not load:
                // load it now (#70). A duplicate in the queue is skipped. A
                // chat that loaded before the reconnect does not load again.
                let waiting = self.open_on_link.as_ref().is_some_and(|(owner, id)| {
                    *owner == protocol
                        && self.selected_protocol == protocol
                        && self.selected_conversation.as_ref() == Some(id)
                });
                if waiting {
                    self.queue_open_chat();
                }
                // A chat-list load that a reconnect ended loads again now
                // (#163).
                if self.list_on_link.remove(&protocol) {
                    self.chat_list_loading.insert(protocol);
                    self.push(AdapterCommand::LoadChats { protocol });
                }
                #[cfg(feature = "whatsapp-web")]
                if self.whatsapp_screen_of(protocol) {
                    self.finish_whatsapp_link();
                }
                #[cfg(feature = "signal-local")]
                if self.signal_screen_of(protocol) {
                    self.signal_removed = false;
                    self.signal_checking = false;
                    self.finish_signal_link();
                }
            }
            AccountState::Unlinked if had_session || before != AccountState::Unlinked => {
                #[cfg(feature = "signal-local")]
                if self.keep_removed_signal(protocol) {
                    self.park_removed_signal();
                } else {
                    self.end_session(protocol);
                }
                #[cfg(not(feature = "signal-local"))]
                self.end_session(protocol);
            }
            AccountState::Unlinked =>
            {
                #[cfg(feature = "signal-local")]
                if self.signal_screen_of(protocol) && self.signal_started {
                    self.signal_started = false;
                    self.signal_inbox_open = false;
                    self.signal_qr = None;
                }
            }
            AccountState::Linking if had_session => self.hold_opens_for_reconnect(protocol),
            AccountState::Linking => {}
        }
    }

    /// New state of a helper process (ADR 0013). A helper that is gone
    /// answers no load: stop the spinners of its protocol. `Idle` counts
    /// too: the adapter sends it when a helper ended and none starts. `Account`
    /// events, not this one, change the link state. A helper that stopped
    /// for good shows what happened, why, and what to do.
    fn set_helper(&mut self, protocol: ProtocolId, state: HelperState) {
        if let Some(row) = self.live_account_mut(protocol) {
            row.helper = Some(state);
        }
        // A publish that was open when the helper left cannot settle.
        // Leave the rows. The next publish starts from an empty set.
        if protocol == ProtocolId::Signal
            && matches!(
                state,
                HelperState::Idle
                    | HelperState::Restarting
                    | HelperState::Stopped(_)
                    | HelperState::Missing
            )
        {
            self.retained_publish = None;
        }
        // The error of an older stop is not true for the new state.
        self.helper_errors.remove(&protocol);
        if matches!(
            state,
            HelperState::Idle
                | HelperState::Restarting
                | HelperState::Stopped(_)
                | HelperState::Missing
        ) {
            self.stop_spinners(protocol, None);
        }
        #[cfg(feature = "whatsapp-web")]
        if self.whatsapp_screen_of(protocol) {
            self.note_whatsapp_helper(state);
        }
        #[cfg(feature = "signal-local")]
        if self.signal_screen_of(protocol) {
            self.note_signal_helper(state);
        }
        if let HelperState::Stopped(fault) = state {
            let name = protocol.display_name();
            let happened = format!("The {name} helper stopped.");
            let (why, next) = match fault {
                HelperFault::Crashed => (
                    "It stopped again each time thinwire started it.".to_owned(),
                    format!(
                        "Click Restart on the {name} account. If it stops again, reinstall thinwire."
                    ),
                ),
                HelperFault::StartFailed => (
                    "The system did not start the helper program.".to_owned(),
                    format!("Reinstall thinwire, then click Restart on the {name} account."),
                ),
                HelperFault::VersionMismatch => (
                    "The helper is from another thinwire version than this app.".to_owned(),
                    "Reinstall thinwire, so the app and the helper have the same version."
                        .to_owned(),
                ),
                HelperFault::SessionInUse => (
                    format!("Another thinwire uses the {name} session on this computer."),
                    format!("Close the other thinwire, then click Restart on the {name} account."),
                ),
                HelperFault::StillRunning => (
                    "The last helper process did not end.".to_owned(),
                    format!(
                        "Wait a moment, then click Restart on the {name} account. If it stays, restart thinwire."
                    ),
                ),
                HelperFault::NoClient => (
                    format!("This helper program has no {name} client."),
                    "Reinstall thinwire. In a local build, build the helper with its client feature."
                        .to_owned(),
                ),
            };
            self.set_error(&happened, &why, &next);
            self.helper_errors.insert(
                protocol,
                UserError {
                    happened,
                    why,
                    next,
                },
            );
        }
    }

    /// A QR or pair code of a helper that is gone is not valid. While the
    /// helper starts again, the pair screen drops it: the new helper sends a
    /// new one for the same pairing. When the helper stopped for good, the
    /// pairing ends, so the user can start it again from the pair screen.
    /// Codes that are used up keep the expired screen.
    #[cfg(feature = "whatsapp-web")]
    fn note_whatsapp_helper(&mut self, state: HelperState) {
        match state {
            HelperState::Restarting => {
                self.whatsapp_qr = None;
                self.whatsapp_pair_code = None;
            }
            HelperState::Stopped(_) | HelperState::Missing => self.stop_whatsapp_pairing(),
            HelperState::Idle | HelperState::Busy | HelperState::Running => {}
        }
    }

    /// The QR of a Signal helper that is gone is not valid. While the helper
    /// starts again, the link screen drops it: the new helper sends a new
    /// one for the same link. When the helper stopped for good, the link
    /// ends, so the user can start it again from the link screen.
    #[cfg(feature = "signal-local")]
    fn note_signal_helper(&mut self, state: HelperState) {
        match state {
            HelperState::Restarting => self.signal_qr = None,
            HelperState::Stopped(_) | HelperState::Missing => {
                self.end_signal_pairing();
                self.signal_started = false;
                self.signal_inbox_open = false;
            }
            HelperState::Idle | HelperState::Busy | HelperState::Running => {}
        }
    }

    /// The helper line and the helper error for a full-screen gate, which
    /// draws no account row and no error block. `None`: nothing to show.
    #[must_use]
    pub fn helper_notice(&self, protocol: ProtocolId) -> Option<HelperNotice<'_>> {
        let row = self.accounts.iter().find(|row| row.caps.id == protocol)?;
        let label = row.helper_label()?;
        Some(HelperNotice {
            label,
            // The helper's own error. The error block is one for the whole
            // app, so another protocol can replace its content.
            error: self.helper_errors.get(&protocol),
        })
    }

    /// The user clicked Restart on an account row. It acts only while the
    /// helper of that protocol is stopped.
    pub fn restart_helper(&mut self, protocol: ProtocolId) {
        let stopped = self
            .accounts
            .iter()
            .any(|row| row.caps.id == protocol && row.helper_stopped());
        if !stopped {
            return;
        }
        self.error = None;
        self.push(AdapterCommand::RestartHelper { protocol });
    }

    /// The user clicked Retry on a keychain denial or a failed move.
    ///
    /// The command asks the helper to read the key again and open the saved
    /// session. A wrong key tries the move-aside again. It is not a new link
    /// and it does not delete a store. A wrong key that already moved does
    /// not take this path.
    pub fn retry_store_key(&mut self, protocol: ProtocolId) {
        let Some(row) = self.live_account_mut(protocol) else {
            return;
        };
        if !matches!(
            row.store_access,
            Some(StoreAccessReason::KeychainDenied | StoreAccessReason::MoveFailed)
        ) {
            return;
        }
        row.store_access = None;
        row.status = AdapterStatus::Connecting;
        row.detail = "Opening the saved session.".into();
        self.push(AdapterCommand::RetryStoreKey { protocol });
    }

    /// Read the session key again for every saved account that was denied.
    /// This is not a new link.
    pub(crate) fn retry_denied_store_keys(&mut self) {
        let protocols: Vec<ProtocolId> = self
            .accounts
            .iter()
            .filter(|row| row.store_access == Some(StoreAccessReason::KeychainDenied))
            .map(|row| row.caps.id)
            .collect();
        for protocol in protocols {
            self.retry_store_key(protocol);
        }
    }

    /// One saved key denial is waiting for a keychain that can be read.
    pub(crate) fn take_store_key_retry_due(&mut self) -> bool {
        std::mem::take(&mut self.store_key_retry_due)
    }

    /// The app keychain state at the start of this pump. A saved-key denial
    /// arms an automatic retry only when this is false.
    pub(crate) fn note_app_keychain_ready(&mut self, ready: bool) {
        self.app_keychain_ready = ready;
    }

    /// The helper program of this protocol is not installed.
    #[must_use]
    pub fn helper_missing(&self, protocol: ProtocolId) -> bool {
        self.accounts
            .iter()
            .any(|row| row.caps.id == protocol && row.helper_missing())
    }

    /// A reconnect starts (`Linking` after `Linked`). An `OpenChat` of this
    /// protocol that is still queued waits for `Linked`, and one in flight
    /// can be lost (#90 items 2 and 7). Mark the selected chat to load on
    /// `Linked`, and drop the queued opens: new commands wait for `Linked`.
    /// Older pages and the chat list of this protocol end too (#163).
    fn hold_opens_for_reconnect(&mut self, protocol: ProtocolId) {
        let is_open = |command: &AdapterCommand| {
            matches!(
                command,
                AdapterCommand::OpenChat { protocol: owner, .. }
                    | AdapterCommand::OpenTopic { protocol: owner, .. }
                    if *owner == protocol
            )
        };
        if self.selected_protocol == protocol
            && let Some(id) = self.selected_conversation.clone()
        {
            let queued = self.pending.iter().any(|command| {
                is_open(command)
                    && match command {
                        AdapterCommand::OpenChat {
                            conversation_id, ..
                        }
                        | AdapterCommand::OpenTopic {
                            conversation_id, ..
                        } => conversation_id == &id,
                        _ => false,
                    }
            });
            let in_flight = self.history_loading.contains(&(protocol, id.clone()));
            if queued || in_flight {
                self.open_on_link = Some((protocol, id));
            }
        }
        // Every other load of this protocol ends here: a dropped open loads
        // nothing, and the reconnect can lose one already in flight. Stop
        // their spinners (qa Low on #111, Codex r4107046484). The marked
        // chat keeps its spinner until Linked loads it again.
        let marked = self.open_on_link.clone();
        self.history_loading.retain(|(owner, chat)| {
            *owner != protocol
                || marked.as_ref().is_some_and(|(marked_owner, marked_chat)| {
                    marked_owner == owner && marked_chat == chat
                })
        });
        self.pending.retain(|command| !is_open(command));
        // An older page in flight can lose its answer too, and a queued one
        // would go out during the reconnect. End both; a later scroll asks
        // again (#163, Codex r4137896062).
        self.older_loading.retain(|key| key.0 != protocol);
        self.pending.retain(|command| {
            !matches!(
                command,
                AdapterCommand::LoadOlderMessages { protocol: owner, .. }
                    | AdapterCommand::LoadOlderTopic { protocol: owner, .. }
                    if *owner == protocol
            )
        });
        // A chat-list load that is in flight or queued ends here too. Its
        // spinner stops, and `Linked` loads the list again (#163).
        let is_list = |command: &AdapterCommand| matches!(command, AdapterCommand::LoadChats { protocol: owner } if *owner == protocol);
        let queued_list = self.pending.iter().any(is_list);
        if self.chat_list_loading.remove(&protocol) || queued_list {
            self.list_on_link.insert(protocol);
        }
        self.pending.retain(|command| !is_list(command));
    }

    /// The live account row of `protocol`. A previous Signal number is a
    /// separate row and does not take link or helper updates.
    fn live_account_mut(&mut self, protocol: ProtocolId) -> Option<&mut AccountRow> {
        self.accounts
            .iter_mut()
            .find(|row| row.caps.id == protocol && row.signal_retained.is_none())
    }

    /// `true` when `id` belongs to the account row that is open.
    fn chat_in_open_account(&self, id: &str) -> bool {
        if !self.selected_protocol.is(ProtocolKind::Signal) {
            return true;
        }
        thinwire_protocol::signal_retained_slot(id) == self.selected_signal_retained
    }

    /// A retained Signal inbox event is local history. It applies with no
    /// live session. A live chat event still waits for a session.
    fn retained_signal_inbox(event: &AdapterEvent) -> bool {
        match event {
            AdapterEvent::ConversationUpsert { conversation } => {
                conversation.protocol == ProtocolId::Signal
                    && thinwire_protocol::signal_retained_slot(&conversation.id).is_some()
            }
            AdapterEvent::MessageReceived { message } => {
                message.protocol == ProtocolId::Signal
                    && thinwire_protocol::signal_retained_slot(&message.conversation_id).is_some()
            }
            AdapterEvent::OlderHistoryLoaded {
                protocol,
                conversation_id,
                ..
            } => {
                *protocol == ProtocolId::Signal
                    && thinwire_protocol::signal_retained_slot(conversation_id).is_some()
            }
            _ => false,
        }
    }

    /// Drop previous-account slots that this publish did not send.
    /// A slot whose store could not be read is in `published` and stays.
    /// Slots that were sent stay, so the rows do not vanish and return.
    fn drop_retained_slots_except(&mut self, published: &HashSet<u32>) {
        let stale: Vec<u32> = self
            .accounts
            .iter()
            .filter_map(|row| row.signal_retained)
            .filter(|slot| !published.contains(slot))
            .collect();
        for slot in stale {
            self.drop_retained_slot(slot);
        }
    }

    fn drop_retained_slot(&mut self, slot: u32) {
        let ids: Vec<String> = self
            .conversations
            .get(&ProtocolId::Signal)
            .into_iter()
            .flatten()
            .filter(|row| thinwire_protocol::signal_retained_slot(&row.id) == Some(slot))
            .map(|row| row.id.clone())
            .collect();
        for id in ids {
            self.remove_conversation(ProtocolId::Signal, &id);
        }
        self.accounts
            .retain(|row| row.signal_retained != Some(slot));
    }

    /// A selected previous account that this link did not publish is closed.
    fn settle_retained_signal_selection(&mut self) {
        if let Some(slot) = self.selected_signal_retained {
            let exists = self.accounts.iter().any(|row| {
                row.caps.id.is(ProtocolKind::Signal) && row.signal_retained == Some(slot)
            });
            if !exists {
                self.selected_signal_retained = None;
                self.selected_conversation = None;
                self.ensure_conversation_selection();
            }
        }
    }

    /// The retained chat of the same conversation, when one is open.
    /// The highest slot wins. The open previous account wins over that.
    /// The slot is a count, not an account id.
    fn retained_chat_for_live(&self, live_id: &str) -> Option<String> {
        let mut best: Option<(u32, String)> = None;
        let rows = self.conversations.get(&ProtocolId::Signal)?;
        for row in rows {
            let Some(slot) = thinwire_protocol::signal_retained_slot(&row.id) else {
                continue;
            };
            if thinwire_protocol::signal_retained_chat(&row.id) != Some(live_id) {
                continue;
            }
            if self.selected_signal_retained == Some(slot) {
                return Some(row.id.clone());
            }
            if best.as_ref().is_none_or(|(kept, _)| slot > *kept) {
                best = Some((slot, row.id.clone()));
            }
        }
        best.map(|(_, id)| id)
    }

    /// Copy kept send text onto the previous-account chat of the same
    /// conversation. A different draft already on that chat stays.
    fn retained_drafts_for(
        &self,
        keep: &[(String, Option<MessageTopic>, String)],
    ) -> Vec<(String, Option<MessageTopic>, String)> {
        keep.iter()
            .filter_map(|(chat, topic, text)| {
                if thinwire_protocol::signal_retained_slot(chat).is_some() {
                    return None;
                }
                let retained = self.retained_chat_for_live(chat)?;
                let current = self.current_text(ProtocolId::Signal, &retained, *topic);
                (current.is_empty() || current == text.as_str()).then_some((
                    retained,
                    *topic,
                    text.clone(),
                ))
            })
            .collect()
    }

    /// Move the live Signal chats out of the inbox. The previous number is
    /// already published under `retained:<slot>:`.
    fn retire_live_signal_chats(&mut self) {
        let live: Vec<String> = self
            .conversations
            .get(&ProtocolId::Signal)
            .into_iter()
            .flatten()
            .filter(|row| thinwire_protocol::signal_retained_slot(&row.id).is_none())
            .map(|row| row.id.clone())
            .collect();
        for id in live {
            self.remove_conversation(ProtocolId::Signal, &id);
        }
    }

    /// Remember one previous-account slot from this publish.
    /// A later chat row clears the unreadable state. The history stays.
    fn note_retained_slot(&mut self, slot: u32) {
        if let Some(published) = &mut self.retained_publish {
            published.insert(slot);
        }
        self.ensure_retained_signal_row(slot);
        let Some(row) = self
            .accounts
            .iter_mut()
            .find(|row| row.caps.id == ProtocolId::Signal && row.signal_retained == Some(slot))
        else {
            return;
        };
        if row.history_unreadable() {
            row.retained_read = RetainedRead::Readable;
            row.detail = thinwire_protocol::SIGNAL_PREVIOUS_ACCOUNT.to_owned();
        }
    }

    /// The store for this slot is still on disk, and this publish could not
    /// read it. Keep the row and the history already on screen.
    fn keep_unreadable_retained_slot(&mut self, slot: u32) {
        if let Some(published) = &mut self.retained_publish {
            published.insert(slot);
        }
        self.ensure_retained_signal_row(slot);
        let Some(row) = self
            .accounts
            .iter_mut()
            .find(|row| row.caps.id == ProtocolId::Signal && row.signal_retained == Some(slot))
        else {
            return;
        };
        row.status = AdapterStatus::Error;
        row.detail = thinwire_protocol::SIGNAL_RETAINED_UNREADABLE.to_owned();
        row.retained_read = RetainedRead::Unreadable;
    }

    /// One read-only row for a previous Signal number. The live row stays.
    fn ensure_retained_signal_row(&mut self, slot: u32) {
        if self
            .accounts
            .iter()
            .any(|row| row.caps.id.is(ProtocolKind::Signal) && row.signal_retained == Some(slot))
        {
            return;
        }
        let Some(caps) = self
            .accounts
            .iter()
            .find(|row| row.caps.id.is(ProtocolKind::Signal) && row.signal_retained.is_none())
            .map(|row| row.caps)
        else {
            return;
        };
        let row = AccountRow {
            caps,
            status: AdapterStatus::Error,
            detail: thinwire_protocol::SIGNAL_PREVIOUS_ACCOUNT.to_owned(),
            state: AccountState::Unlinked,
            helper: None,
            signal_retained: Some(slot),
            store_access: None,
            retained_read: RetainedRead::Readable,
        };
        // The highest slot is the most recent previous account. It sits
        // next to the live row. "Previous account N" stays the slot.
        let at = self
            .accounts
            .iter()
            .position(|row| {
                row.caps.id.is(ProtocolKind::Signal)
                    && row.signal_retained.is_some_and(|existing| existing < slot)
            })
            .unwrap_or_else(|| {
                self.accounts
                    .iter()
                    .rposition(|row| row.caps.id.is(ProtocolKind::Signal))
                    .map_or(self.accounts.len(), |index| index + 1)
            });
        self.accounts.insert(at, row);
    }

    /// The phone removed this Signal device. Keep the account row, the open
    /// chat, and the local history. Do not ask before that, and do not delete
    /// it. Sending is off. A later `Unlinked` from a cancel uses the same
    /// path while this flag stays set.
    #[cfg(feature = "signal-local")]
    fn keep_removed_signal(&self, protocol: ProtocolId) -> bool {
        protocol == ProtocolId::Signal && self.signal_removed
    }

    /// Stop the Signal worker state without dropping chats or messages.
    #[cfg(feature = "signal-local")]
    fn park_removed_signal(&mut self) {
        if let Some(row) = self.live_account_mut(ProtocolId::Signal) {
            row.state = AccountState::Unlinked;
            row.status = AdapterStatus::Error;
            row.detail = thinwire_protocol::SIGNAL_DEVICE_REMOVED.to_owned();
        }
        self.stop_spinners(ProtocolId::Signal, None);
        self.signal_started = false;
        self.signal_inbox_open = false;
        self.end_signal_pairing();
        self.signal_screen = SignalScreen::Hidden;
        self.signal_removed = true;
        self.signal_checking = false;
        // The account ended. An open retained publish must not settle later.
        self.retained_publish = None;
        // The line is on the row and in the open chat. No error block, and
        // the strip does not repeat it.
        self.error = None;
        self.clear_linked_signal_status();
    }

    /// Drop a status line that still says this Signal device is linked.
    #[cfg(feature = "signal-local")]
    fn clear_linked_signal_status(&mut self) {
        if self.status_text.to_ascii_lowercase().contains("linked") {
            self.status_text.clear();
        }
    }

    /// One unauthorized open is waiting for a confirm open. The row shows
    /// [`thinwire_protocol::SIGNAL_LINK_CHECK`]. This is not removal.
    #[must_use]
    pub fn signal_link_checking(&self) -> bool {
        #[cfg(feature = "signal-local")]
        {
            self.signal_checking
        }
        #[cfg(not(feature = "signal-local"))]
        {
            false
        }
    }

    /// The phone removed this Signal device. The row and the open chat show
    /// [`thinwire_protocol::SIGNAL_DEVICE_REMOVED`]. Sending is off.
    #[must_use]
    pub fn signal_device_removed(&self) -> bool {
        #[cfg(feature = "signal-local")]
        {
            self.signal_removed
        }
        #[cfg(not(feature = "signal-local"))]
        {
            false
        }
    }

    /// The session of one protocol ended (shell plan 8): drop its rows,
    /// messages, spinners, notes, and sends. Another linked protocol takes
    /// the selection. Drafts of this protocol go too, except a rejected send
    /// whose text is still in that chat. A Telegram session end drops those
    /// as well: the next account on this machine must not see them.
    fn end_session(&mut self, protocol: ProtocolId) {
        if let Some(row) = self.live_account_mut(protocol) {
            row.state = AccountState::Unlinked;
        }
        self.sessions.remove(&protocol);
        // Read the rejected text first. Retiring a live Signal chat removes
        // that text, and the previous-account row must still show it.
        // Telegram drops every draft (the next account on this machine must
        // not see them). Another protocol keeps a rejected send's text, and
        // the text of a send still in flight (#102, #135).
        let mut keep = self.texts_to_keep_on_session_end(protocol);
        if protocol == ProtocolId::Signal {
            keep.extend(self.retained_drafts_for(&keep));
            // The live number ends. Previous numbers stay. Their history
            // is in `retained/` on this computer.
            self.retire_live_signal_chats();
            // An open publish must not settle after this and drop a row.
            self.retained_publish = None;
        } else {
            self.conversations.remove(&protocol);
        }
        self.rejected_bodies
            .retain(|(owner, _), _| *owner != protocol);
        self.drafts.retain(|(owner, _, _), _| *owner != protocol);
        self.typing.forget(protocol);
        // The files of an ended session do not go to a new one.
        self.attachments
            .retain(|(owner, _, _), _| *owner != protocol);
        for (chat, topic, body) in keep {
            self.drafts.insert((protocol, chat, topic), body);
        }
        if protocol != ProtocolId::Signal {
            self.messages.retain(|(owner, _, _), _| *owner != protocol);
            self.topics.retain(|(owner, _), _| *owner != protocol);
        }
        self.outbox_read.retain(|(owner, _), _| *owner != protocol);
        if self.selected_protocol == protocol {
            self.selected_topic = None;
        }
        self.history_loading.retain(|(owner, _)| *owner != protocol);
        self.chat_list_loading.remove(&protocol);
        self.list_on_link.remove(&protocol);
        // A download in flight dies with the session. Forget it, and drop a
        // queued one, so the next sign-in can request that file again.
        self.photo_downloads.retain(|(owner, _)| *owner != protocol);
        self.file_downloads.retain(|(owner, _)| *owner != protocol);
        self.pending.retain(|command| {
            !matches!(
                command,
                AdapterCommand::DownloadFile { protocol: owner, .. }
                    | AdapterCommand::DownloadVoice { protocol: owner, .. }
                    | AdapterCommand::DownloadDocument { protocol: owner, .. }
                    if *owner == protocol
            )
        });
        self.notices.remove(&protocol);
        self.sends.drop_protocol(protocol);
        // Its timed-out sends leave the timeout error too (#90).
        let shown = self.error.is_some() && self.error == self.timeout_error;
        self.timed_out.retain(|entry| entry.protocol != protocol);
        if shown {
            self.show_timeouts();
        }
        self.forget_older_protocol(protocol);
        self.end_telegram_send(protocol, TelegramSendEnd::All);
        #[cfg(feature = "whatsapp-web")]
        if self.whatsapp_screen_of(protocol) {
            // The adapter ended the pairing itself (a pair code that
            // failed, or a ban). QR codes that ran out keep the expired
            // screen: `Unlinked` follows that status.
            self.stop_whatsapp_pairing();
        }
        #[cfg(feature = "signal-local")]
        if self.signal_screen_of(protocol) {
            self.signal_started = false;
            self.signal_inbox_open = false;
            self.signal_checking = false;
            self.end_signal_pairing();
            // The acceptance is of the notice text, not of this session.
            // The saved hash stays. The next launch can start the helper.
            self.signal_notice_acknowledged = false;
            self.signal_screen = SignalScreen::Hidden;
        }
        // A previous Signal number stays selected. Its rows and history
        // stay. The live row shows its own signed-out state.
        let stay_on_retained =
            protocol == ProtocolId::Signal && self.selected_signal_retained.is_some();
        if self.selected_protocol == protocol && !stay_on_retained {
            self.compose.clear();
            self.selected_conversation = None;
            if let Some(next) = self
                .accounts
                .iter()
                .find(|row| {
                    self.has_session(row.caps.id) && self.account_surface_visible(row.caps.id)
                })
                .map(|row| row.caps.id)
            {
                self.select_protocol(next);
            }
        } else if stay_on_retained
            && self
                .selected_conversation
                .as_deref()
                .is_some_and(|id| thinwire_protocol::signal_retained_slot(id).is_none())
        {
            self.compose.clear();
            self.selected_conversation = None;
            self.ensure_conversation_selection();
        }
    }

    /// A Signal status error while linking is still in progress.
    /// After `Ready`, the worker is already linked: `begin_signal_link`
    /// would reject another start, so the error is a session error.
    #[cfg(feature = "signal-local")]
    fn note_signal_failure(&mut self, detail: &str) {
        if self.signal_inbox_open {
            // SendRejected already explained a failed send. Keep that hint.
            let send_already_shown = detail.starts_with("Signal send failed")
                && self
                    .error
                    .as_ref()
                    .is_some_and(|error| error.happened == "Message not sent.");
            if !send_already_shown {
                self.command_failed(ProtocolId::Signal, None, detail);
            }
            return;
        }
        let happened = if self.protocol_linked(ProtocolId::Signal) {
            "Signal did not finish opening the inbox."
        } else {
            "Signal linking failed."
        };
        self.set_error(happened, detail, "Start linking again.");
    }

    /// One command failed; the session is still up (shell plan 7). Stop its
    /// spinner and show the error. The rows, the selection, the link state,
    /// and any send in flight stay.
    fn command_failed(&mut self, protocol: ProtocolId, chat: Option<&str>, detail: &str) {
        self.stop_spinners(protocol, chat);
        let happened = format!(
            "{}: the last action did not finish.",
            protocol.display_name()
        );
        self.set_error(&happened, detail, "Try again. The inbox stays open.");
    }

    /// Stop the loading spinners of one protocol: one chat, or all of them.
    fn stop_spinners(&mut self, protocol: ProtocolId, chat: Option<&str>) {
        match chat {
            Some(chat) => {
                let key = (protocol, chat.to_owned());
                self.history_loading.remove(&key);
                self.older_loading
                    .retain(|page| !(page.0 == protocol && page.1 == chat));
            }
            None => {
                self.chat_list_loading.remove(&protocol);
                self.history_loading.retain(|(owner, _)| *owner != protocol);
                self.older_loading.retain(|page| page.0 != protocol);
            }
        }
    }

    fn remove_conversation(&mut self, protocol: ProtocolId, id: &str) {
        if let Some(list) = self.conversations.get_mut(&protocol) {
            list.retain(|row| row.id != id);
        }
        self.messages
            .retain(|key, _| !(key.0 == protocol && key.1 == id));
        self.outbox_read.remove(&(protocol, id.to_owned()));
        self.topics.remove(&(protocol, id.to_owned()));
        // Every removed chat loses its draft, selected or not (#43).
        self.drafts
            .retain(|key, _| !(key.0 == protocol && key.1 == id));
        self.drop_rejected_body(protocol, id);
        self.forget_older_chat(protocol, id);
        if self.selected_protocol == protocol && self.selected_conversation.as_deref() == Some(id) {
            self.compose.clear();
            self.selected_conversation = None;
            self.selected_topic = None;
            self.ensure_conversation_selection();
        }
        self.sync_focused_row(FocusFollow::List);
    }

    /// Keep the highlight on a visible row.
    ///
    /// Set `scroll_to_focused` when the highlight moves by key or when the
    /// search text changes. A chat update keeps the id and does not scroll.
    fn sync_focused_row(&mut self, reason: FocusFollow) {
        let ids = self.visible_ids();
        let kept = self
            .focused_row
            .as_ref()
            .is_some_and(|id| ids.iter().any(|row| row == id));
        if !kept {
            self.focused_row = None;
        } else if self.highlight_needs_scroll(reason, &ids) {
            self.scroll_to_focused = true;
        }
        self.seen_visible_ids = ids;
    }

    fn highlight_needs_scroll(&self, reason: FocusFollow, ids: &[String]) -> bool {
        match reason {
            FocusFollow::Query | FocusFollow::Moved => true,
            FocusFollow::List => self.highlight_index_changed(ids),
            FocusFollow::Upsert => false,
        }
    }

    fn highlight_index_changed(&self, ids: &[String]) -> bool {
        let Some(id) = self.focused_row.as_deref() else {
            return false;
        };
        let old = self.seen_visible_ids.iter().position(|row| row == id);
        let new = ids.iter().position(|row| row == id);
        old.is_some() && new.is_some() && old != new
    }

    fn visible_ids(&self) -> Vec<String> {
        self.visible_conversations()
            .iter()
            .map(|row| row.id.clone())
            .collect()
    }

    /// The highlight, when it is one of the rows on screen. Enter uses this.
    #[must_use]
    pub fn visible_focused_row(&self) -> Option<String> {
        let id = self.focused_row.clone()?;
        self.visible_conversations()
            .iter()
            .any(|row| row.id == id)
            .then_some(id)
    }

    fn message_bucket(
        &self,
        protocol: ProtocolId,
        conversation_id: &str,
        message_id: &str,
    ) -> Option<(ProtocolId, String, Option<MessageTopic>)> {
        self.messages.iter().find_map(|(key, list)| {
            (key.0 == protocol
                && key.1 == conversation_id
                && list.iter().any(|row| row.id == message_id))
            .then(|| key.clone())
        })
    }

    fn delivery_of(
        &self,
        protocol: ProtocolId,
        conversation_id: &str,
        id: &str,
    ) -> Option<Delivery> {
        let key = self.message_bucket(protocol, conversation_id, id)?;
        self.messages
            .get(&key)?
            .iter()
            .find(|row| row.id == id)
            .map(|row| row.delivery)
    }

    fn set_delivery(
        &mut self,
        protocol: ProtocolId,
        conversation_id: &str,
        message_id: &str,
        delivery: Delivery,
    ) {
        let before = self.delivery_of(protocol, conversation_id, message_id);
        let last_read = self
            .outbox_read
            .get(&(protocol, conversation_id.to_owned()))
            .copied();
        let Some(key) = self.message_bucket(protocol, conversation_id, message_id) else {
            return;
        };
        let Some(message) = self
            .messages
            .get_mut(&key)
            .and_then(|list| list.iter_mut().find(|row| row.id == message_id))
        else {
            return;
        };
        message.delivery = delivery;
        // Incoming and outgoing Telegram rows share one id space. The cursor
        // marks only a row this account sent.
        if message.outbound
            && let Some(last_read) = last_read
            && let Some(id) = Self::message_number(&message.id)
        {
            message.delivery = with_outbox_read(message.delivery, id, last_read);
        }
        let message = message.clone();
        self.note_delivery(before, &message);
    }

    /// The numeric id at the end of `telegram:<chat>:<id>`. Other id shapes
    /// are not a Telegram message id.
    fn message_number(id: &str) -> Option<i64> {
        let number = id.rsplit_once(':')?.1;
        if number.is_empty() || number.starts_with('+') {
            return None;
        }
        number.parse().ok()
    }

    /// Raise the read cursor and mark outgoing rows the other party has read.
    /// An incoming row keeps its delivery.
    fn note_outbox_read(&mut self, protocol: ProtocolId, conversation_id: &str, last_read: i64) {
        let key = (protocol, conversation_id.to_owned());
        let last = {
            let stored = self.outbox_read.entry(key).or_insert(0);
            if last_read > *stored {
                *stored = last_read;
            }
            *stored
        };
        let keys: Vec<_> = self
            .messages
            .keys()
            .filter(|key| key.0 == protocol && key.1 == conversation_id)
            .cloned()
            .collect();
        let mut read = Vec::new();
        for key in keys {
            let Some(list) = self.messages.get_mut(&key) else {
                continue;
            };
            for message in list.iter_mut() {
                if !message.outbound {
                    continue;
                }
                let Some(id) = Self::message_number(&message.id) else {
                    continue;
                };
                let next = with_outbox_read(message.delivery, id, last);
                if next != message.delivery {
                    message.delivery = next;
                    if next == Delivery::Read {
                        read.push(message.id.clone());
                    }
                }
            }
        }
        if protocol.is(ProtocolKind::Telegram) {
            for id in read {
                self.release_telegram_for_message(&id);
            }
        }
    }

    /// A sent outbound row the other party has already read shows read.
    /// An incoming row keeps its delivery.
    fn apply_outbox_cursor(&self, message: &mut ChatMessage) {
        if !message.outbound {
            return;
        }
        let Some(last) = self
            .outbox_read
            .get(&(message.protocol, message.conversation_id.clone()))
            .copied()
        else {
            return;
        };
        let Some(id) = Self::message_number(&message.id) else {
            return;
        };
        message.delivery = with_outbox_read(message.delivery, id, last);
    }

    /// A send that was pending and now failed: show the error block and keep the text.
    fn note_delivery(&mut self, before: Option<Delivery>, message: &ChatMessage) {
        if !message.outbound
            || message.delivery != Delivery::Failed
            || before != Some(Delivery::Pending)
        {
            return;
        }
        self.release_telegram_for_message(&message.id);
        let selected =
            self.is_selected_thread(message.protocol, &message.conversation_id, message.topic);
        if selected {
            if self.compose.trim().is_empty() {
                self.compose.clone_from(&message.body);
            }
        } else {
            self.drafts
                .entry((
                    message.protocol,
                    message.conversation_id.clone(),
                    message.topic,
                ))
                .or_insert_with(|| message.body.clone());
        }
        let why = format!(
            "{} did not accept the message.",
            message.protocol.display_name()
        );
        self.set_error(
            "Message not sent.",
            &why,
            "Press Retry on the message, or edit the text and send it again.",
        );
    }

    /// A live message in a topic that is not on screen adds one to that topic.
    /// The open topic stays at zero. A topic the list does not know yet waits
    /// for the next topic load.
    fn note_live_topic(&mut self, message: &ChatMessage) {
        if message.arrival != thinwire_protocol::Arrival::Live || message.outbound {
            return;
        }
        let Some(topic) = message.topic else {
            return;
        };
        if self.window_focused
            && self.topic_is_viewed(message.protocol, &message.conversation_id, topic)
        {
            return;
        }
        let Some(rows) = self
            .topics
            .get_mut(&(message.protocol, message.conversation_id.clone()))
        else {
            return;
        };
        let Some(row) = rows.iter_mut().find(|row| row.topic == topic) else {
            return;
        };
        row.unread = row.unread.saturating_add(1);
        if message.sent_at > row.last_at {
            row.last_at = message.sent_at;
        }
    }

    fn upsert_topic(&mut self, mut topic: ChatTopic) {
        if self.window_focused
            && self.topic_is_viewed(topic.protocol, &topic.conversation_id, topic.topic)
        {
            topic.unread = 0;
        }
        let list = self
            .topics
            .entry((topic.protocol, topic.conversation_id.clone()))
            .or_default();
        if let Some(existing) = list.iter_mut().find(|row| row.topic == topic.topic) {
            *existing = topic;
        } else {
            list.push(topic);
        }
    }

    fn topic_is_viewed(&self, protocol: ProtocolId, id: &str, topic: MessageTopic) -> bool {
        self.viewed.as_ref().is_some_and(|(owner, chat, open)| {
            *owner == protocol && chat == id && *open == Some(topic)
        })
    }

    fn upsert_message(&mut self, mut message: ChatMessage) {
        self.apply_outbox_cursor(&mut message);
        let key = thread_key(message.protocol, &message.conversation_id, message.topic);
        let list = self.messages.entry(key).or_default();
        if let Some(existing) = list.iter_mut().find(|row| row.id == message.id) {
            Self::keep_voice_file(&mut message.audio, existing.audio.as_ref());
            Self::keep_video_file(&mut message.video, existing.video.as_ref());
            Self::keep_document(&mut message.file, existing.file.as_ref());
            *existing = message;
        } else {
            list.push(message);
        }
        sort_messages(list);
    }

    fn remove_message(&mut self, protocol: ProtocolId, conversation_id: &str, message_id: &str) {
        let Some(key) = self.message_bucket(protocol, conversation_id, message_id) else {
            return;
        };
        let Some(list) = self.messages.get_mut(&key) else {
            return;
        };
        list.retain(|row| row.id != message_id);
    }

    fn remove_messages(
        &mut self,
        protocol: ProtocolId,
        conversation_id: &str,
        message_ids: &[String],
    ) {
        let keys: Vec<_> = self
            .messages
            .keys()
            .filter(|key| key.0 == protocol && key.1 == conversation_id)
            .cloned()
            .collect();
        for key in keys {
            let Some(list) = self.messages.get_mut(&key) else {
                continue;
            };
            list.retain(|row| !message_ids.contains(&row.id));
        }
    }

    fn mark_edited(&mut self, protocol: ProtocolId, conversation_id: &str, message_id: &str) {
        if let Some(message) = self.message_mut(protocol, conversation_id, message_id) {
            message.edited = true;
        }
    }

    fn patch_message_body(
        &mut self,
        protocol: ProtocolId,
        conversation_id: &str,
        message_id: &str,
        body: String,
    ) {
        let Some(key) = self.message_bucket(protocol, conversation_id, message_id) else {
            return;
        };
        let Some(list) = self.messages.get_mut(&key) else {
            return;
        };
        if let Some(message) = list.iter_mut().find(|row| row.id == message_id) {
            message.body = body;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn patch_message_media(
        &mut self,
        protocol: ProtocolId,
        conversation_id: &str,
        message_id: &str,
        body: String,
        links: Vec<thinwire_protocol::MessageLink>,
        photo: Option<thinwire_protocol::MessagePhoto>,
        audio: Option<MessageAudio>,
        video: Option<thinwire_protocol::MessageVideo>,
        file: Option<thinwire_protocol::MessageFile>,
    ) {
        let Some(message) = self.message_mut(protocol, conversation_id, message_id) else {
            return;
        };
        let stored = message.audio.clone();
        let stored_video = message.video.clone();
        let stored_file = message.file.clone();
        message.body = body;
        message.links = links;
        message.photo = photo;
        message.audio = audio;
        message.video = video;
        message.file = file;
        Self::keep_voice_file(&mut message.audio, stored.as_ref());
        Self::keep_video_file(&mut message.video, stored_video.as_ref());
        Self::keep_document(&mut message.file, stored_file.as_ref());
    }

    /// As [`Self::keep_voice_file`], for a video.
    fn keep_video_file(
        incoming: &mut Option<thinwire_protocol::MessageVideo>,
        stored: Option<&thinwire_protocol::MessageVideo>,
    ) {
        let (Some(incoming), Some(stored)) = (incoming.as_mut(), stored) else {
            return;
        };
        if incoming.file_id() != stored.file_id() || incoming.file().is_some() {
            return;
        }
        if let Some(path) = stored.file().cloned() {
            incoming.set_file(path);
        }
        if stored.failed() {
            incoming.mark_failed();
        }
    }

    fn keep_document(
        incoming: &mut Option<thinwire_protocol::MessageFile>,
        stored: Option<&thinwire_protocol::MessageFile>,
    ) {
        let (Some(incoming), Some(stored)) = (incoming.as_mut(), stored) else {
            return;
        };
        if incoming.file_id() != stored.file_id() || incoming.file().is_some() {
            return;
        }
        if let Some(path) = stored.file().cloned() {
            incoming.set_file(path);
        }
        if stored.failed() {
            incoming.mark_failed();
        }
    }

    fn keep_voice_file(incoming: &mut Option<MessageAudio>, stored: Option<&MessageAudio>) {
        let Some(stored) = stored else {
            return;
        };
        let Some(incoming) = incoming.as_mut() else {
            return;
        };
        if incoming.file_id() != stored.file_id() || incoming.file().is_some() {
            return;
        }
        if let Some(path) = stored.file().cloned() {
            incoming.set_file(path);
        }
        match stored.stop() {
            Some(stop) => incoming.mark_stopped(stop),
            None => incoming.clear_failed(),
        }
    }

    fn patch_message_links(
        &mut self,
        protocol: ProtocolId,
        conversation_id: &str,
        message_id: &str,
        links: Vec<thinwire_protocol::MessageLink>,
    ) {
        let Some(message) = self.message_mut(protocol, conversation_id, message_id) else {
            return;
        };
        message.links = links;
    }

    /// Download one photo that is on screen. A second request while the first
    /// is in flight does nothing. `PhotoFailed`, or the session ending, lets
    /// a later request try again.
    pub(crate) fn load_photo(
        &mut self,
        protocol: ProtocolId,
        conversation_id: &str,
        message_id: &str,
        file_id: i32,
    ) {
        if !self.is_selected_thread(protocol, conversation_id, self.selected_topic) {
            return;
        }
        let Some(key) = self.message_bucket(protocol, conversation_id, message_id) else {
            return;
        };
        if key.2 != self.selected_topic {
            return;
        }
        let (known, ready, secret) = {
            let Some(message) = self.message_mut(protocol, conversation_id, message_id) else {
                return;
            };
            let mut known = false;
            let mut ready = false;
            let mut secret = false;
            let photos = [
                message.photo.as_mut(),
                message
                    .file
                    .as_mut()
                    .and_then(thinwire_protocol::MessageFile::preview_mut),
            ];
            for photo in photos.into_iter().flatten() {
                let hit = photo.sizes.iter().any(|size| size.file_id == Some(file_id));
                let done = photo
                    .sizes
                    .iter()
                    .any(|size| size.file_id == Some(file_id) && size.ready());
                if hit && !done {
                    photo.failed = false;
                }
                if hit {
                    known = true;
                    ready = done;
                    secret = photo.is_secret;
                }
            }
            (known, ready, secret)
        };
        if !known || ready || !self.photo_downloads.insert((protocol, file_id)) {
            return;
        }
        self.queue(AdapterCommand::DownloadFile {
            protocol,
            file_id,
            secret,
        });
    }

    pub(crate) fn play_audio(
        &mut self,
        protocol: ProtocolId,
        conversation_id: &str,
        message_id: &str,
        file_id: i32,
    ) {
        // Every adapter that sets audio on a message can download it.
        if !self.is_selected_thread(protocol, conversation_id, self.selected_topic) {
            return;
        }
        let Some(key) = self.message_bucket(protocol, conversation_id, message_id) else {
            return;
        };
        if key.2 != self.selected_topic {
            return;
        }
        {
            let Some(message) = self.message_mut(protocol, conversation_id, message_id) else {
                return;
            };
            let Some(audio) = message.audio.as_mut() else {
                return;
            };
            if audio.file_id() != file_id || audio.file().is_some() {
                return;
            }
            audio.clear_failed();
        }
        self.queue(AdapterCommand::DownloadVoice { protocol, file_id });
    }

    /// Download the small photo of an inbox row. A second request while the
    /// first is in flight does nothing. The same `DownloadFile` path as a
    /// chat photo. [`ProtocolId::downloads_avatar`] names who can ask.
    pub(crate) fn load_avatar(
        &mut self,
        protocol: ProtocolId,
        conversation_id: &str,
        file_id: i32,
    ) {
        if !protocol.downloads_avatar() {
            return;
        }
        {
            let Some(list) = self.conversations.get_mut(&protocol) else {
                return;
            };
            let Some(row) = list.iter_mut().find(|row| row.id == conversation_id) else {
                return;
            };
            if row.avatar.file_id != Some(file_id) || row.avatar.ready() || row.avatar.failed {
                return;
            }
            // The row remembers the ask, so the inbox does not send it each frame.
            row.avatar.requested = true;
        }
        if !self.photo_downloads.insert((protocol, file_id)) {
            return;
        }
        self.queue(AdapterCommand::DownloadFile {
            protocol,
            file_id,
            secret: false,
        });
    }

    /// Store a finished download. `file` has a path or bytes.
    fn photo_ready(
        &mut self,
        protocol: ProtocolId,
        file_id: i32,
        file: thinwire_protocol::PhotoFile,
    ) {
        let path = file.path().cloned();
        let bytes = file.bytes().cloned();
        debug_assert!(
            path.is_some() || bytes.is_some(),
            "PhotoFile always has a path or bytes"
        );
        self.photo_downloads.remove(&(protocol, file_id));
        for list in self.messages.values_mut() {
            for message in list.iter_mut() {
                if message.protocol != protocol {
                    continue;
                }
                let Some(photo) = message.photo.as_mut() else {
                    continue;
                };
                let mut hit = false;
                for size in &mut photo.sizes {
                    if size.file_id != Some(file_id) {
                        continue;
                    }
                    hit = true;
                    if photo.is_secret {
                        size.path = None;
                        size.bytes = bytes.clone();
                    } else {
                        size.path = path.clone();
                        if bytes.is_some() {
                            size.bytes = bytes.clone();
                        }
                    }
                }
                if hit {
                    photo.failed = false;
                }
            }
        }
        for message in self.messages.values_mut().flatten() {
            if message.protocol != protocol {
                continue;
            }
            let Some(preview) = message.file.as_mut().and_then(|file| file.preview_mut()) else {
                continue;
            };
            let mut hit = false;
            for size in &mut preview.sizes {
                if size.file_id != Some(file_id) {
                    continue;
                }
                hit = true;
                size.path.clone_from(&path);
                if bytes.is_some() {
                    size.bytes.clone_from(&bytes);
                }
            }
            if hit {
                preview.failed = false;
            }
        }
        let Some(list) = self.conversations.get_mut(&protocol) else {
            return;
        };
        for row in list.iter_mut() {
            if row.avatar.file_id != Some(file_id) {
                continue;
            }
            row.avatar.failed = false;
            if let Some(bytes) = &bytes {
                row.avatar.bytes = Some(bytes.clone());
            }
            if path.is_some() {
                row.avatar.path.clone_from(&path);
            }
        }
    }

    fn photo_failed(&mut self, protocol: ProtocolId, file_id: i32) {
        self.photo_downloads.remove(&(protocol, file_id));
        for list in self.messages.values_mut() {
            for message in list.iter_mut() {
                if message.protocol != protocol {
                    continue;
                }
                let Some(photo) = message.photo.as_mut() else {
                    continue;
                };
                if photo.sizes.iter().any(|size| size.file_id == Some(file_id)) {
                    photo.failed = true;
                }
            }
        }
        for message in self.messages.values_mut().flatten() {
            if message.protocol != protocol {
                continue;
            }
            let Some(preview) = message.file.as_mut().and_then(|file| file.preview_mut()) else {
                continue;
            };
            if preview
                .sizes
                .iter()
                .any(|size| size.file_id == Some(file_id))
            {
                preview.failed = true;
            }
        }
        let Some(list) = self.conversations.get_mut(&protocol) else {
            return;
        };
        for row in list.iter_mut() {
            if row.avatar.file_id == Some(file_id) {
                row.avatar.failed = true;
            }
        }
    }

    /// `FileFailed`, or the session ending, lets a later request try again.
    pub(crate) fn download_document(
        &mut self,
        protocol: ProtocolId,
        conversation_id: &str,
        message_id: &str,
        file_id: i32,
    ) {
        if !self.is_selected_thread(protocol, conversation_id, self.selected_topic) {
            return;
        }
        let Some(key) = self.message_bucket(protocol, conversation_id, message_id) else {
            return;
        };
        if key.2 != self.selected_topic {
            return;
        }
        {
            let Some(message) = self.message_mut(protocol, conversation_id, message_id) else {
                return;
            };
            let Some(file) = message.file.as_mut() else {
                return;
            };
            if file.file_id() != file_id || file.file().is_some() {
                return;
            }
            file.clear_failed();
        }
        if !self.file_downloads.insert((protocol, file_id)) {
            return;
        }
        self.queue(AdapterCommand::DownloadDocument { protocol, file_id });
    }

    fn file_ready(&mut self, protocol: ProtocolId, file_id: i32, path: AudioPath) {
        self.file_downloads.remove(&(protocol, file_id));
        for message in self.messages.values_mut().flatten() {
            if message.protocol != protocol {
                continue;
            }
            if let Some(file) = message.file.as_mut()
                && file.file_id() == file_id
            {
                file.set_file(path.clone());
            }
        }
    }

    fn file_failed(&mut self, protocol: ProtocolId, file_id: i32) {
        self.file_downloads.remove(&(protocol, file_id));
        for message in self.messages.values_mut().flatten() {
            if message.protocol != protocol {
                continue;
            }
            if let Some(file) = message.file.as_mut()
                && file.file_id() == file_id
            {
                file.mark_failed();
            }
        }
    }

    fn voice_ready(&mut self, protocol: ProtocolId, file_id: i32, path: AudioPath) {
        for list in self.messages.values_mut() {
            for message in list.iter_mut() {
                if message.protocol != protocol {
                    continue;
                }
                let Some(audio) = message.audio.as_mut() else {
                    continue;
                };
                if audio.file_id() == file_id {
                    audio.set_file(path.clone());
                }
            }
        }
    }

    fn voice_failed(&mut self, protocol: ProtocolId, file_id: i32, stop: VoiceStop) {
        for list in self.messages.values_mut() {
            for message in list.iter_mut() {
                if message.protocol != protocol {
                    continue;
                }
                let Some(audio) = message.audio.as_mut() else {
                    continue;
                };
                if audio.file_id() == file_id {
                    audio.mark_stopped(stop);
                }
            }
        }
    }

    fn video_ready(&mut self, protocol: ProtocolId, file_id: i32, path: AudioPath) {
        for message in self.messages.values_mut().flatten() {
            if message.protocol != protocol {
                continue;
            }
            if let Some(video) = message.video.as_mut()
                && video.file_id() == file_id
            {
                video.set_file(path.clone());
            }
        }
    }

    fn video_failed(&mut self, protocol: ProtocolId, file_id: i32) {
        for message in self.messages.values_mut().flatten() {
            if message.protocol != protocol {
                continue;
            }
            if let Some(video) = message.video.as_mut()
                && video.file_id() == file_id
            {
                video.mark_failed();
            }
        }
    }

    /// Ask the adapter for the video of an open message (ADR 0015). The
    /// same guards as [`Self::play_audio`].
    pub(crate) fn play_video(
        &mut self,
        protocol: ProtocolId,
        conversation_id: &str,
        message_id: &str,
        file_id: i32,
    ) {
        if !self.is_selected_thread(protocol, conversation_id, self.selected_topic) {
            return;
        }
        let Some(key) = self.message_bucket(protocol, conversation_id, message_id) else {
            return;
        };
        if key.2 != self.selected_topic {
            return;
        }
        {
            let Some(message) = self.message_mut(protocol, conversation_id, message_id) else {
                return;
            };
            let Some(video) = message.video.as_mut() else {
                return;
            };
            if video.file_id() != file_id || video.file().is_some() {
                return;
            }
            video.clear_failed();
        }
        self.queue(AdapterCommand::DownloadVideo { protocol, file_id });
    }

    fn message_mut(
        &mut self,
        protocol: ProtocolId,
        conversation_id: &str,
        message_id: &str,
    ) -> Option<&mut ChatMessage> {
        let key = self.message_bucket(protocol, conversation_id, message_id)?;
        self.messages
            .get_mut(&key)?
            .iter_mut()
            .find(|row| row.id == message_id)
    }

    /// WhatsApp pairing chrome when feature `whatsapp-web` is compiled.
    ///
    /// The entry does not wait for a linked Telegram account. The default
    /// build leaves the feature off, so first-run chrome stays Telegram-only.
    #[cfg(feature = "whatsapp-web")]
    #[must_use]
    pub fn whatsapp_pairing_available(&self) -> bool {
        self.account_surface_visible(ProtocolId::WhatsApp)
    }

    #[cfg(feature = "whatsapp-web")]
    #[must_use]
    pub fn whatsapp_gate_open(&self) -> bool {
        self.whatsapp_pairing_available() && !matches!(self.whatsapp_screen, WhatsAppScreen::Hidden)
    }

    #[cfg(feature = "whatsapp-web")]
    pub fn open_whatsapp_risk_gate(&mut self) {
        // With no helper program, no pairing can start: the entry is
        // disabled, and a frontend cannot open the gate another way.
        if !self.whatsapp_pairing_available() || self.helper_missing(self.whatsapp_link_account) {
            return;
        }
        if self.whatsapp_started {
            self.push(AdapterCommand::WhatsAppCancelLink);
        }
        self.whatsapp_screen = WhatsAppScreen::RiskGate;
        self.end_pairing();
        self.whatsapp_started = false;
        self.whatsapp_risk_acknowledged = false;
        self.whatsapp_phone.clear();
        self.whatsapp_phone_note = None;
    }

    #[cfg(feature = "whatsapp-web")]
    /// Leave the gate or the pair screen. After the risk was accepted this is
    /// Cancel: pairing stops, the phone vault clears, and the acknowledgement
    /// resets (PR #48 review). From the gate alone it only hides the screen.
    pub fn close_whatsapp_gate(&mut self, phone: &WhatsAppPhoneVault) {
        if self.whatsapp_screen == WhatsAppScreen::Pair
            || self.whatsapp_risk_acknowledged
            || self.whatsapp_started
        {
            self.cancel_whatsapp_link(phone);
            return;
        }
        self.whatsapp_screen = WhatsAppScreen::Hidden;
        self.whatsapp_phone.clear();
        self.whatsapp_phone_note = None;
        phone.clear();
    }

    /// The user accepted the ban gate of this pairing.
    #[must_use]
    pub const fn whatsapp_gate_accepted(&self) -> bool {
        #[cfg(feature = "whatsapp-web")]
        {
            self.whatsapp_risk_acknowledged
        }
        #[cfg(not(feature = "whatsapp-web"))]
        {
            false
        }
    }

    /// The app starts with a saved acceptance of the current ban gate. The
    /// helper starts and opens a saved, paired session. No screen opens, and
    /// a new link still shows the gate.
    #[cfg(feature = "whatsapp-web")]
    pub(crate) fn restore_whatsapp_gate(&mut self) {
        if self.helper_missing(ProtocolId::WhatsApp) {
            return;
        }
        self.push(AdapterCommand::WhatsAppAcknowledgeRisk);
    }

    #[cfg(feature = "whatsapp-web")]
    pub fn acknowledge_whatsapp_risk(&mut self) {
        // The ban gate must be on screen: a frontend cannot skip it.
        if self.whatsapp_screen != WhatsAppScreen::RiskGate {
            tracing::warn!("whatsapp risk acknowledgement dropped: the risk gate is not shown");
            return;
        }
        self.whatsapp_risk_acknowledged = true;
        self.whatsapp_screen = WhatsAppScreen::Pair;
        self.error = None;
        self.status_text = WHATSAPP_GATE_ACCEPTED.into();
        self.push(AdapterCommand::WhatsAppAcknowledgeRisk);
    }

    /// New text in the phone field of the pair screen.
    #[cfg(feature = "whatsapp-web")]
    pub fn set_whatsapp_phone(&mut self, value: String) {
        self.whatsapp_phone = value;
        self.whatsapp_phone_note = None;
    }

    #[cfg(feature = "whatsapp-web")]
    pub fn begin_whatsapp_link(&mut self, phone: &WhatsAppPhoneVault) {
        // Pairing starts only from the pair screen, after the gate was accepted.
        if self.whatsapp_screen != WhatsAppScreen::Pair || !self.whatsapp_risk_acknowledged {
            tracing::warn!("whatsapp pairing dropped: the risk gate was not accepted");
            return;
        }
        // A pairing that already runs stays as it is. The exception is an
        // exhausted QR: "Get new code" starts a new pairing generation.
        let fresh = !self.whatsapp_started;
        if self.whatsapp_started && !self.whatsapp_qr_exhausted {
            return;
        }
        // The helper program can go away while the pair screen is open. No
        // helper can take the pairing then, and no answer would end it: do
        // not start. The pair screen shows the reason.
        if self.helper_missing(self.whatsapp_link_account) {
            return;
        }
        // A phone number has 15 digits at most. A longer text is not one,
        // and it must not go to the helper. The text stays, so the user can
        // correct it. A restart keeps the number already in the vault: the
        // field was cleared on the first start, and writing it again would
        // replace that number with an empty field.
        if fresh {
            if self.whatsapp_phone.chars().count() > WHATSAPP_PHONE_MAX_CHARS {
                self.whatsapp_phone_note = Some(WHATSAPP_PHONE_TOO_LONG);
                return;
            }
            self.whatsapp_phone_note = None;
            phone.set_phone(&self.whatsapp_phone);
            self.whatsapp_phone.clear();
        }
        self.whatsapp_qr_exhausted = false;
        self.whatsapp_qr = None;
        self.whatsapp_pair_code = None;
        self.whatsapp_started = true;
        self.error = None;
        self.status_text = WHATSAPP_PAIRING_REQUESTED.into();
        let generation = self.next_pairing_generation;
        self.next_pairing_generation += 1;
        self.pairing_generation
            .insert(self.whatsapp_link_account, generation);
        self.push(AdapterCommand::WhatsAppBeginLink { generation });
    }

    /// Leave the QR and open the phone pair-code field.
    ///
    /// The pair screen stays open and the ban gate stays accepted. A pairing
    /// that is running stops, so the phone field can start a new one.
    #[cfg(feature = "whatsapp-web")]
    pub fn open_whatsapp_phone_link(&mut self, phone: &WhatsAppPhoneVault) {
        if self.whatsapp_screen != WhatsAppScreen::Pair || !self.whatsapp_risk_acknowledged {
            tracing::warn!("whatsapp phone link dropped: the risk gate was not accepted");
            return;
        }
        if self.helper_missing(self.whatsapp_link_account) {
            return;
        }
        if self.whatsapp_started {
            self.push(AdapterCommand::WhatsAppCancelLink);
        }
        phone.clear();
        self.end_pairing();
        self.whatsapp_started = false;
        self.whatsapp_phone.clear();
        self.whatsapp_phone_note = None;
    }

    /// The shell id of the WhatsApp pairing that runs now. `0` when none runs.
    /// The pair screen caches the QR image under this id.
    #[cfg(feature = "whatsapp-web")]
    #[must_use]
    pub fn whatsapp_pairing_generation(&self) -> u64 {
        self.pairing_generation
            .get(&self.whatsapp_link_account)
            .copied()
            .unwrap_or(0)
    }

    /// WhatsApp linked (`Account { Linked }`): close the pair screen the user
    /// started and drop the pairing material. A reconnect also links; it
    /// must not close a risk gate the user just opened.
    #[cfg(feature = "whatsapp-web")]
    fn finish_whatsapp_link(&mut self) {
        // A line about the pairing is not true after the link. This also
        // applies when the pair screen closed before the phone linked.
        if [WHATSAPP_PAIRING_REQUESTED, WHATSAPP_GATE_ACCEPTED].contains(&self.status_text.as_str())
        {
            self.status_text = WHATSAPP_LINKED.into();
        }
        // The pair screen closes on any link: the pairing the user started,
        // or the saved session that opened after the gate. A Cancel on a
        // pair screen that stays open would end that session. A reconnect
        // does not close the risk gate: that is another screen.
        if self.whatsapp_screen != WhatsAppScreen::Pair {
            return;
        }
        self.whatsapp_screen = WhatsAppScreen::Hidden;
        self.end_pairing();
        self.whatsapp_phone.clear();
        self.status_text = WHATSAPP_LINKED.into();
        // Another account linked: the screens are for account 0 again.
        self.whatsapp_link_account = ProtocolId::WhatsApp;
    }

    /// The pairing stopped. Drop its QR and its pair code.
    ///
    /// When the QR codes are used up, the expired screen stays.
    /// `whatsapp_started` stays true, so Get new code does not write the
    /// empty phone field over the vault number. Any other stop enables
    /// Start again.
    #[cfg(feature = "whatsapp-web")]
    fn stop_whatsapp_pairing(&mut self) {
        let exhausted = self.whatsapp_qr_exhausted;
        self.end_pairing();
        if exhausted {
            self.whatsapp_qr_exhausted = true;
        } else {
            self.whatsapp_started = false;
        }
    }

    /// No pairing runs any more: drop the shown QR and pair code, and every
    /// later payload of the old pairing.
    #[cfg(feature = "whatsapp-web")]
    fn end_pairing(&mut self) {
        self.pairing_generation.remove(&self.whatsapp_link_account);
        self.whatsapp_qr = None;
        self.whatsapp_pair_code = None;
        self.whatsapp_qr_exhausted = false;
    }

    /// A pairing payload belongs to the pairing of `protocol` that runs now.
    #[cfg(any(feature = "whatsapp-web", feature = "signal-local"))]
    fn current_pairing(&self, protocol: ProtocolId, generation: u64) -> bool {
        let current = self.pairing_generation.get(&protocol) == Some(&generation);
        if !current {
            tracing::warn!("pairing payload dropped: it is from an older pairing");
        }
        current
    }

    #[cfg(feature = "whatsapp-web")]
    pub fn cancel_whatsapp_link(&mut self, phone: &WhatsAppPhoneVault) {
        phone.clear();
        self.whatsapp_phone.clear();
        self.whatsapp_phone_note = None;
        self.end_pairing();
        self.whatsapp_started = false;
        self.whatsapp_risk_acknowledged = false;
        self.whatsapp_screen = WhatsAppScreen::Hidden;
        self.status_text = "WhatsApp pairing cancelled.".into();
        self.push(AdapterCommand::WhatsAppCancelLink);
        self.whatsapp_link_account = ProtocolId::WhatsApp;
    }

    /// Signal linking chrome when feature `signal-local` is compiled.
    ///
    /// The default build leaves the feature off, so first-run chrome stays
    /// Telegram-only.
    #[cfg(feature = "signal-local")]
    #[must_use]
    pub fn signal_linking_available(&self) -> bool {
        self.account_surface_visible(ProtocolId::Signal)
    }

    #[cfg(feature = "signal-local")]
    #[must_use]
    pub fn signal_gate_open(&self) -> bool {
        self.signal_linking_available() && !matches!(self.signal_screen, SignalScreen::Hidden)
    }

    /// No Signal link runs any more: drop the shown QR, and every later
    /// payload of the old link.
    #[cfg(feature = "signal-local")]
    fn end_signal_pairing(&mut self) {
        self.pairing_generation.remove(&self.signal_link_account);
        self.signal_qr = None;
    }

    /// The user accepted the current Signal notice copy.
    #[cfg(feature = "signal-local")]
    #[must_use]
    pub const fn signal_notice_acknowledged(&self) -> bool {
        self.signal_notice_acknowledged
    }

    /// Cancel or an explicit re-show asked to drop the saved hash. One read.
    #[cfg(feature = "signal-local")]
    pub(crate) fn take_signal_notice_forget(&mut self) -> bool {
        std::mem::take(&mut self.signal_notice_forget)
    }

    /// A saved acceptance of the current notice copy. The notice stays off
    /// the screen. The helper starts. A link still starts only from the link
    /// screen.
    #[cfg(feature = "signal-local")]
    pub(crate) fn restore_signal_notice(&mut self) {
        self.signal_notice_acknowledged = true;
        self.push(AdapterCommand::SignalAcknowledgeNotice);
    }

    /// The saved notice hash still matches. Link must not show the notice
    /// again. This does not send a command and does not clear the hash.
    #[cfg(feature = "signal-local")]
    pub(crate) fn keep_saved_signal_notice(&mut self) {
        self.signal_notice_acknowledged = true;
    }

    /// The user opened the notice.
    ///
    /// A linked account shows the notice and keeps the saved acceptance.
    /// A remembered acceptance that is not already on screen also keeps
    /// that acceptance. Link then opens the link screen. The account is
    /// not linked yet after a keychain failure, and after a helper crash
    /// that has not reported `Linked` again. That Link click must not drop
    /// the saved hash. Cancel drops it. A re-show from the link screen
    /// drops it too.
    #[cfg(feature = "signal-local")]
    pub fn open_signal_notice(&mut self) {
        if self.signal_notice_acknowledged && self.signal_screen == SignalScreen::Hidden {
            if !self.signal_linking_available() || self.helper_missing(self.signal_link_account) {
                return;
            }
            self.signal_qr = None;
            self.signal_screen = if self.protocol_linked(self.signal_link_account) {
                SignalScreen::Notice
            } else {
                SignalScreen::Link
            };
            return;
        }
        self.reveal_signal_notice(true);
    }

    /// The saved hash is for an older notice copy. Show that notice. The
    /// saved hash stays until the user accepts the current copy or clears it.
    #[cfg(feature = "signal-local")]
    pub(crate) fn show_outdated_signal_notice(&mut self) {
        self.reveal_signal_notice(false);
    }

    #[cfg(feature = "signal-local")]
    fn reveal_signal_notice(&mut self, forget: bool) {
        // With no helper program the entry is disabled (ADR 0013 decision
        // 6), and a frontend cannot open the notice another way.
        if !self.signal_linking_available() || self.helper_missing(self.signal_link_account) {
            return;
        }
        if self.protocol_linked(self.signal_link_account) {
            self.signal_screen = SignalScreen::Notice;
            self.signal_qr = None;
            return;
        }
        if self.signal_started {
            self.push(AdapterCommand::SignalCancelLink);
        }
        self.signal_screen = SignalScreen::Notice;
        self.signal_qr = None;
        self.signal_started = false;
        self.signal_inbox_open = false;
        self.signal_notice_acknowledged = false;
        if forget {
            self.signal_notice_forget = true;
        }
    }

    #[cfg(feature = "signal-local")]
    /// Leave the notice or the link screen. After the notice was accepted
    /// this is Cancel: linking stops and the acknowledgement resets. From
    /// the notice alone it only hides the screen.
    pub fn close_signal_gate(&mut self) {
        if self.protocol_linked(self.signal_link_account) {
            self.signal_screen = SignalScreen::Hidden;
            self.signal_qr = None;
            return;
        }
        if self.signal_screen == SignalScreen::Link
            || self.signal_notice_acknowledged
            || self.signal_started
        {
            self.cancel_signal_link();
            return;
        }
        self.signal_screen = SignalScreen::Hidden;
    }

    #[cfg(feature = "signal-local")]
    pub fn acknowledge_signal_notice(&mut self) {
        // The notice must be on screen: a frontend cannot skip it.
        if self.signal_screen != SignalScreen::Notice {
            tracing::warn!("signal notice acknowledgement dropped: the notice is not shown");
            return;
        }
        if self.protocol_linked(self.signal_link_account) {
            self.signal_screen = SignalScreen::Hidden;
            self.signal_qr = None;
            return;
        }
        self.signal_notice_acknowledged = true;
        self.signal_screen = SignalScreen::Link;
        self.error = None;
        self.status_text = "Signal notice accepted. Linking has not started.".into();
        self.push(AdapterCommand::SignalAcknowledgeNotice);
    }

    /// The Link button after the phone removed this device. The notice was
    /// already accepted, so this opens the link screen and starts the QR.
    /// With no acceptance yet, it opens the notice first.
    #[cfg(feature = "signal-local")]
    pub fn relink_removed_signal(&mut self) {
        // The same path after the phone removed the device and after a wrong
        // key moved the store files aside. Neither path deletes those files.
        let wrong_key = self.accounts.iter().any(|row| {
            row.caps.id.is(ProtocolKind::Signal)
                && row.store_access == Some(StoreAccessReason::WrongKey)
                && !row.linked()
        });
        if (!self.signal_removed && !wrong_key)
            || !self.signal_linking_available()
            || self.helper_missing(self.signal_link_account)
        {
            return;
        }
        if !self.signal_notice_acknowledged {
            self.open_signal_notice();
            return;
        }
        self.signal_screen = SignalScreen::Link;
        self.signal_started = false;
        self.error = None;
        self.begin_signal_link();
    }

    #[cfg(feature = "signal-local")]
    pub fn begin_signal_link(&mut self) {
        // Linking starts only from the link screen, after the notice was accepted.
        if self.signal_screen != SignalScreen::Link || !self.signal_notice_acknowledged {
            tracing::warn!("signal linking dropped: the notice was not accepted");
            return;
        }
        if self.signal_started {
            return;
        }
        // The helper program can go away while the link screen is open. No
        // helper can take the link then, and no answer would end it: do not
        // start. The link screen shows the reason.
        if self.helper_missing(self.signal_link_account) {
            return;
        }
        self.signal_started = true;
        self.signal_inbox_open = false;
        self.error = None;
        self.status_text = "Signal linking requested.".into();
        let generation = self.next_pairing_generation;
        self.next_pairing_generation += 1;
        self.pairing_generation
            .insert(self.signal_link_account, generation);
        self.push(AdapterCommand::SignalBeginLink { generation });
    }

    /// The shell id of the Signal link that runs now. `0` when none runs.
    /// The link screen caches the QR image under this id.
    #[cfg(feature = "signal-local")]
    #[must_use]
    pub fn signal_pairing_generation(&self) -> u64 {
        self.pairing_generation
            .get(&self.signal_link_account)
            .copied()
            .unwrap_or(0)
    }

    /// Leave the link screen after `Account { Linked }`. The worker stays up.
    /// This does not send `SignalCancelLink`.
    #[cfg(feature = "signal-local")]
    pub fn finish_signal_link(&mut self) {
        if self.signal_screen == SignalScreen::Hidden {
            return;
        }
        self.signal_screen = SignalScreen::Hidden;
        self.end_signal_pairing();
        self.status_text = "Signal is linked.".into();
        self.select_protocol(self.signal_link_account);
        // Another account linked: the screens are for account 0 again.
        self.signal_link_account = ProtocolId::Signal;
    }

    #[cfg(feature = "signal-local")]
    pub fn cancel_signal_link(&mut self) {
        self.end_signal_pairing();
        self.signal_started = false;
        self.signal_inbox_open = false;
        self.signal_notice_acknowledged = false;
        // A cancel for another account keeps the saved acceptance of
        // account 0 (ADR 0014).
        self.signal_notice_forget |= self.signal_link_account.slot() == 0;
        self.signal_screen = SignalScreen::Hidden;
        self.status_text = "Signal linking cancelled.".into();
        self.push(AdapterCommand::SignalCancelLink);
        self.signal_link_account = ProtocolId::Signal;
    }
}

/// What an unlink does for an account of `kind`, in one line. A frontend
/// shows it in the question before the unlink.
#[must_use]
pub const fn unlink_notice(kind: ProtocolKind) -> &'static str {
    match kind {
        ProtocolKind::Telegram => {
            "thinwire logs this computer out of Telegram. Telegram deletes its data on this computer. Your Telegram account and your other devices stay."
        }
        ProtocolKind::WhatsApp => {
            "thinwire asks WhatsApp to remove this linked device. It deletes the session and the saved chats on this computer. Your WhatsApp account stays."
        }
        ProtocolKind::Signal => {
            "thinwire asks Signal to remove this linked device. It deletes the session on this computer. If your phone still shows the device, remove it there: Settings > Linked devices."
        }
        ProtocolKind::Discord => {
            "thinwire removes the bot token from the keychain. The bot stays in its servers."
        }
        ProtocolKind::Slack => {
            "thinwire removes the workspace tokens from the keychain. The Slack app stays installed in the workspace."
        }
    }
}

/// An own text message that the server has: Edit offers it. The adapters
/// edit text only, not a caption.
#[must_use]
pub fn can_edit(message: &ChatMessage) -> bool {
    message.outbound
        && !message.body.trim().is_empty()
        && matches!(message.delivery, Delivery::Sent | Delivery::Read)
        && message.photo.is_none()
        && message.audio.is_none()
        && message.video.is_none()
        && message.file.is_none()
}

fn sort_conversations(list: &mut [Conversation]) {
    list.sort_by_key(|row| (std::cmp::Reverse(row.order), row.id.clone()));
}

fn sort_messages(list: &mut [ChatMessage]) {
    // Send time first: some protocols (WhatsApp) use ids with no order.
    // Numeric ids rank the rest when the time is unknown (zero).
    list.sort_by_key(|row| (row.sent_at, rank_within_second(row)));
}

fn rank_within_second(message: &ChatMessage) -> i64 {
    if message.protocol.is(ProtocolKind::Signal)
        && let Some(millis) = thinwire_protocol::signal_store_millis(&message.id)
        && let Ok(rank) = i64::try_from(millis)
    {
        rank
    } else {
        message_rank(&message.id)
    }
}

fn message_rank(id: &str) -> i64 {
    id.rsplit(':')
        .next()
        .and_then(|part| part.parse().ok())
        .unwrap_or(0)
}

fn persist_api(
    store: &SecretStore,
    api_id: &str,
    api_hash: &str,
) -> Result<(), crate::secrets::SecretError> {
    store.set(SecretKey::ApiId, api_id)?;
    store.set(SecretKey::ApiHash, api_hash)?;
    Ok(())
}

fn clear_ephemeral(store: &SecretStore) {
    for key in SecretKey::EPHEMERAL {
        store.set_secret(key, "");
    }
}

#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod test_support {
    //! Fixtures shared by the core tests and the egui source-text tests.

    use super::*;

    pub fn submit_and_apply(
        snapshot: &mut Snapshot,
        store: &SecretStore,
        phase: TelegramAuthPhase,
    ) {
        snapshot.advance_telegram(store);
        snapshot.apply(AdapterEvent::TelegramAuth { phase });
    }

    pub fn seed_override(store: &SecretStore) {
        store.set(SecretKey::ApiId, "11111").expect("id");
        store.set(SecretKey::ApiHash, "hash-value").expect("hash");
    }

    pub fn complete_telegram(snapshot: &mut Snapshot, store: &SecretStore) {
        seed_override(store);
        snapshot.open_telegram(store);
        snapshot.apply(AdapterEvent::TelegramAuth {
            phase: TelegramAuthPhase::NeedPhone,
        });
        assert_eq!(snapshot.auth, AuthScreen::TelegramPhone);
        snapshot.telegram_phone = "+15551234567".into();
        submit_and_apply(snapshot, store, TelegramAuthPhase::NeedCode);
        assert_eq!(snapshot.auth, AuthScreen::TelegramCode);
        snapshot.telegram_code = "12345".into();
        submit_and_apply(snapshot, store, TelegramAuthPhase::NeedTwoFactor);
        assert_eq!(snapshot.auth, AuthScreen::Telegram2fa);
        snapshot.telegram_2fa = "2fa-secret".into();
        submit_and_apply(snapshot, store, TelegramAuthPhase::Ready);
        assert_eq!(snapshot.auth, AuthScreen::Idle);
        assert!(snapshot.telegram_ready());
    }

    pub fn telegram_chat(id: i64, title: &str, order: i64) -> Conversation {
        Conversation {
            protocol: ProtocolId::Telegram,
            id: format!("telegram:{id}"),
            title: title.into(),
            participant: title.into(),
            preview: String::new(),
            unread: 0,
            order,
            last_at: 0,
            is_group: false,
            writable: true,
            placeholder: false,
            muted: false,
            has_topics: false,
            avatar: Default::default(),
        }
    }

    /// Telegram signed in, as after Ready: the flag and the linked row.
    pub fn link_telegram(snapshot: &mut Snapshot) {
        snapshot.telegram_authorized = true;
        if let Some(row) = snapshot
            .accounts
            .iter_mut()
            .find(|row| row.caps.id.is(ProtocolKind::Telegram))
        {
            row.state = AccountState::Linked;
        }
    }

    /// Show the account row of `protocol` in a build with its feature off,
    /// as the demo does.
    pub fn show_protocol(snapshot: &mut Snapshot, protocol: ProtocolId) {
        snapshot.hidden_for_test.remove(&protocol);
        snapshot.extra_visible.insert(protocol);
    }

    /// Show the account chrome of these protocols only, also in a build
    /// with other protocol features on. A test of one protocol then has the
    /// same layout in every feature build (ADR 0013: one CI job turns on
    /// every release feature).
    pub fn show_only(snapshot: &mut Snapshot, protocols: &[ProtocolId]) {
        for protocol in ProtocolId::ALL {
            if protocols.contains(&protocol) {
                snapshot.hidden_for_test.remove(&protocol);
            } else {
                snapshot.extra_visible.remove(&protocol);
                snapshot.hidden_for_test.insert(protocol);
            }
        }
    }

    pub fn ready_with_chats(store: &SecretStore) -> Snapshot {
        let mut snapshot = Snapshot::new();
        complete_telegram(&mut snapshot, store);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: telegram_chat(1, "Ada", 10),
        });
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: telegram_chat(2, "Bob", 5),
        });
        snapshot.take_commands();
        snapshot
    }

    pub fn auth_steps(snapshot: &mut Snapshot) -> Vec<TelegramAuthStep> {
        snapshot
            .take_commands()
            .into_iter()
            .filter_map(|command| match command {
                // Any epoch: the host stamps the real one later (PR #49 review).
                AdapterCommand::TelegramAuth { step, epoch: _ } => Some(step),
                _ => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    /// End the start-up resume with no saved session. A `telegram-tdlib`
    /// build shows the resume screen until the keychain read settles, so a
    /// test of the first-run screen settles it first. The test then holds in
    /// every feature build (ADR 0013: one CI job turns on every release
    /// feature).
    fn settle_resume(snapshot: &mut Snapshot) {
        snapshot.poll_resume(&SecretStore::memory());
    }

    /// A start with no Signal or WhatsApp account does not read a session key.
    #[test]
    fn startup_with_no_signal_or_whatsapp_account_does_not_read_a_session_key() {
        let mut snapshot = Snapshot::new();
        snapshot.poll_resume(&SecretStore::memory());
        assert!(!snapshot.has_session(ProtocolId::Signal));
        assert!(!snapshot.has_session(ProtocolId::WhatsApp));
        let commands = snapshot.take_commands();
        assert!(
            commands
                .iter()
                .all(|command| !thinwire_protocol::command_reads_session_key(command)),
            "a start with no such account must not read the key: {commands:?}"
        );
    }

    fn signal_ada_chat() -> Conversation {
        let mut chat = telegram_chat(1, "Ada", 1);
        chat.protocol = ProtocolId::Signal;
        chat.id = "signal:ada".into();
        chat
    }

    fn link_signal(snapshot: &mut Snapshot) {
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Signal,
            state: AccountState::Linked,
        });
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: signal_ada_chat(),
        });
        snapshot.take_commands();
    }

    /// A saved account hears the retry sentence. A first link hears
    /// "did not link". Retry reads the key again on both, and does not
    /// start a link.
    #[test]
    fn a_denied_key_says_did_not_link_only_on_a_first_link() {
        let mut saved = Snapshot::new();
        link_signal(&mut saved);
        saved.apply(AdapterEvent::StoreAccess {
            protocol: ProtocolId::Signal,
            reason: StoreAccessReason::KeychainDenied,
        });
        let row = saved
            .accounts
            .iter()
            .find(|row| row.caps.id == ProtocolId::Signal)
            .expect("signal row");
        assert_eq!(row.state, AccountState::Linked);
        assert_eq!(row.detail, thinwire_protocol::KEYCHAIN_ACCESS_DENIED);
        assert!(
            !row.detail.contains("did not link"),
            "a saved account does not hear that the link failed"
        );
        assert_eq!(row.store_access, Some(StoreAccessReason::KeychainDenied));
        saved.retry_store_key(ProtocolId::Signal);
        assert_eq!(
            saved.take_commands(),
            vec![AdapterCommand::RetryStoreKey {
                protocol: ProtocolId::Signal,
            }]
        );

        let mut first = Snapshot::new();
        first.apply(AdapterEvent::Account {
            protocol: ProtocolId::Signal,
            state: AccountState::Linking,
        });
        first.take_commands();
        first.apply(AdapterEvent::StoreAccess {
            protocol: ProtocolId::Signal,
            reason: StoreAccessReason::KeychainDenied,
        });
        let row = first
            .accounts
            .iter()
            .find(|row| row.caps.id == ProtocolId::Signal)
            .expect("signal row");
        assert_ne!(row.state, AccountState::Linked);
        assert_eq!(row.detail, thinwire_protocol::signal_keychain_unavailable());
        assert!(row.detail.contains("did not link"));
        assert_eq!(row.store_access, Some(StoreAccessReason::KeychainDenied));
        first.retry_store_key(ProtocolId::Signal);
        assert_eq!(
            first.take_commands(),
            vec![AdapterCommand::RetryStoreKey {
                protocol: ProtocolId::Signal,
            }]
        );

        let mut saved_status = Snapshot::new();
        link_signal(&mut saved_status);
        saved_status.apply(AdapterEvent::Status {
            protocol: ProtocolId::Signal,
            status: AdapterStatus::Error,
            detail: thinwire_protocol::signal_key_not_saved().into(),
        });
        let row = saved_status
            .accounts
            .iter()
            .find(|row| row.caps.id == ProtocolId::Signal)
            .expect("signal row");
        assert!(
            !row.detail.contains("did not link"),
            "a saved account keeps the reason without the first-link sentence"
        );
        let mut first_status = Snapshot::new();
        first_status.apply(AdapterEvent::Status {
            protocol: ProtocolId::Signal,
            status: AdapterStatus::Error,
            detail: thinwire_protocol::signal_key_not_saved().into(),
        });
        let row = first_status
            .accounts
            .iter()
            .find(|row| row.caps.id == ProtocolId::Signal)
            .expect("signal row");
        assert!(row.detail.contains("did not link"));
    }

    /// Keychain Deny keeps the row, the chats, and the exact sentence.
    /// Retry asks for the key again and does not start a link.
    #[test]
    fn a_denied_keychain_read_keeps_the_row_and_retry_reads_the_key() {
        let mut snapshot = Snapshot::new();
        link_signal(&mut snapshot);
        snapshot.apply(AdapterEvent::StoreAccess {
            protocol: ProtocolId::Signal,
            reason: StoreAccessReason::KeychainDenied,
        });
        let row = snapshot
            .accounts
            .iter()
            .find(|row| row.caps.id == ProtocolId::Signal)
            .expect("signal row");
        assert_eq!(row.state, AccountState::Linked);
        assert_eq!(row.status, AdapterStatus::Error);
        assert_eq!(row.detail, thinwire_protocol::KEYCHAIN_ACCESS_DENIED);
        assert_eq!(row.store_access, Some(StoreAccessReason::KeychainDenied));
        assert!(
            snapshot.error.is_none(),
            "a denied read is not a link failure"
        );
        assert_eq!(
            snapshot
                .conversations
                .get(&ProtocolId::Signal)
                .map(Vec::len),
            Some(1),
            "the account is not emptied"
        );
        snapshot.retry_store_key(ProtocolId::Signal);
        assert_eq!(
            snapshot.take_commands(),
            vec![AdapterCommand::RetryStoreKey {
                protocol: ProtocolId::Signal,
            }]
        );
        let row = snapshot
            .accounts
            .iter()
            .find(|row| row.caps.id == ProtocolId::Signal)
            .expect("signal row");
        assert_eq!(row.status, AdapterStatus::Connecting);
        assert!(row.store_access.is_none());
        assert_eq!(row.state, AccountState::Linked);
    }

    /// A wrong key leaves Linked. The sentence ends with Link again.
    /// Retry does nothing. Link starts the same QR flow as a removed device.
    #[test]
    fn a_wrong_key_leaves_linked_and_link_starts_the_relink() {
        for (protocol, sentence) in [
            (ProtocolId::Signal, thinwire_protocol::signal_session_kept()),
            (
                ProtocolId::WhatsApp,
                thinwire_protocol::whatsapp_session_kept(),
            ),
        ] {
            let mut snapshot = Snapshot::new();
            snapshot.apply(AdapterEvent::Account {
                protocol,
                state: AccountState::Linked,
            });
            snapshot.apply(AdapterEvent::ConversationUpsert {
                conversation: {
                    let mut chat = telegram_chat(1, "Ada", 1);
                    chat.protocol = protocol;
                    chat.id = format!("{protocol:?}:ada");
                    chat
                },
            });
            snapshot.take_commands();
            assert!(snapshot.has_session(protocol));
            snapshot.apply(AdapterEvent::StoreAccess {
                protocol,
                reason: StoreAccessReason::WrongKey,
            });
            let row = snapshot
                .accounts
                .iter()
                .find(|row| row.caps.id == protocol)
                .expect("row");
            assert_eq!(row.state, AccountState::Unlinked);
            assert!(!snapshot.has_session(protocol));
            assert_eq!(row.detail, sentence);
            assert!(row.detail.ends_with("Link again"), "{}", row.detail);
            assert_eq!(row.store_access, Some(StoreAccessReason::WrongKey));
            snapshot.retry_store_key(protocol);
            assert!(
                snapshot.take_commands().is_empty(),
                "a wrong key does not retry the key read"
            );
            #[cfg(feature = "signal-local")]
            if protocol == ProtocolId::Signal {
                snapshot.signal_notice_acknowledged = true;
                snapshot.relink_removed_signal();
                let commands = snapshot.take_commands();
                assert!(
                    commands
                        .iter()
                        .any(|command| matches!(command, AdapterCommand::SignalBeginLink { .. })),
                    "Link starts the QR flow: {commands:?}"
                );
                assert!(
                    commands
                        .iter()
                        .all(|command| matches!(command, AdapterCommand::SignalBeginLink { .. })),
                    "Link does not delete the saved store: {commands:?}"
                );
            }
            #[cfg(feature = "whatsapp-web")]
            if protocol == ProtocolId::WhatsApp {
                snapshot.open_whatsapp_risk_gate();
                assert!(matches!(snapshot.whatsapp_screen, WhatsAppScreen::RiskGate));
                assert!(
                    snapshot.take_commands().is_empty(),
                    "the risk gate does not delete the saved store"
                );
            }
        }
    }

    /// A failed move is not a wrong key. The files stayed, the row stays
    /// linked, and Retry asks for the move again. It does not link.
    #[test]
    fn a_failed_move_keeps_the_linked_row_and_retries_the_move() {
        for (protocol, sentence) in [
            (
                ProtocolId::Signal,
                thinwire_protocol::signal_key_wrong_kept(),
            ),
            (
                ProtocolId::WhatsApp,
                thinwire_protocol::whatsapp_key_wrong_kept(),
            ),
        ] {
            let mut snapshot = Snapshot::new();
            snapshot.apply(AdapterEvent::Account {
                protocol,
                state: AccountState::Linked,
            });
            snapshot.apply(AdapterEvent::ConversationUpsert {
                conversation: {
                    let mut chat = telegram_chat(1, "Ada", 1);
                    chat.protocol = protocol;
                    chat.id = format!("{protocol:?}:ada");
                    chat
                },
            });
            snapshot.take_commands();
            snapshot.apply(AdapterEvent::Account {
                protocol,
                state: AccountState::Linking,
            });
            snapshot.apply(AdapterEvent::Status {
                protocol,
                status: AdapterStatus::Error,
                detail: sentence.into(),
            });
            let row = snapshot
                .accounts
                .iter()
                .find(|row| row.caps.id == protocol)
                .expect("row");
            assert_eq!(row.state, AccountState::Linked);
            assert!(snapshot.has_session(protocol));
            assert_eq!(row.store_access, Some(StoreAccessReason::MoveFailed));
            assert_eq!(row.detail, sentence);
            assert!(row.detail.contains("left in place"), "{}", row.detail);
            assert!(
                row.detail.contains("Nothing was deleted."),
                "{}",
                row.detail
            );
            assert!(!row.detail.contains("Link again"), "{}", row.detail);
            assert_eq!(
                snapshot.conversations.get(&protocol).map(Vec::len),
                Some(1),
                "the account is not emptied"
            );
            snapshot.retry_store_key(protocol);
            let commands = snapshot.take_commands();
            assert_eq!(
                commands,
                vec![AdapterCommand::RetryStoreKey { protocol }],
                "Retry reads the key and tries the move again"
            );
            assert!(
                commands
                    .iter()
                    .all(|command| matches!(command, AdapterCommand::RetryStoreKey { .. })),
                "Retry does not start a link: {commands:?}"
            );
        }
    }

    // region: helper process (ADR 0013)

    fn helper(snapshot: &mut Snapshot, state: HelperState) {
        snapshot.apply(AdapterEvent::Helper {
            protocol: ProtocolId::WhatsApp,
            state,
        });
    }

    fn whatsapp_row(snapshot: &Snapshot) -> &AccountRow {
        snapshot
            .accounts
            .iter()
            .find(|row| row.caps.id == ProtocolId::WhatsApp)
            .expect("whatsapp row")
    }

    /// #246 UX: when the helper stops for good, the account row shows
    /// "Helper stopped." with Restart, and the error block says what
    /// happened, why, and what to do.
    #[test]
    fn a_stopped_helper_shows_on_the_account_row_with_what_to_do() {
        let mut snapshot = Snapshot::new();
        assert_eq!(whatsapp_row(&snapshot).helper_label(), None);
        helper(&mut snapshot, HelperState::Stopped(HelperFault::Crashed));
        let row = whatsapp_row(&snapshot);
        assert!(row.helper_stopped());
        assert_eq!(row.helper_label(), Some("Helper stopped."));
        let error = snapshot.error.clone().expect("error block");
        assert_eq!(error.happened, "The WhatsApp helper stopped.");
        assert_eq!(error.why, "It stopped again each time thinwire started it.");
        assert_eq!(
            error.next,
            "Click Restart on the WhatsApp account. If it stops again, reinstall thinwire."
        );
        // No other row changes.
        assert!(
            snapshot
                .accounts
                .iter()
                .filter(|row| row.caps.id != ProtocolId::WhatsApp)
                .all(|row| row.helper.is_none())
        );
    }

    #[test]
    fn each_helper_fault_names_its_own_fix() {
        for (fault, next) in [
            (
                HelperFault::StartFailed,
                "Reinstall thinwire, then click Restart on the WhatsApp account.",
            ),
            (
                HelperFault::VersionMismatch,
                "Reinstall thinwire, so the app and the helper have the same version.",
            ),
            (
                HelperFault::SessionInUse,
                "Close the other thinwire, then click Restart on the WhatsApp account.",
            ),
            (
                HelperFault::StillRunning,
                "Wait a moment, then click Restart on the WhatsApp account. If it stays, restart thinwire.",
            ),
            (
                HelperFault::NoClient,
                "Reinstall thinwire. In a local build, build the helper with its client feature.",
            ),
        ] {
            let mut snapshot = Snapshot::new();
            helper(&mut snapshot, HelperState::Stopped(fault));
            assert_eq!(snapshot.error.expect("error block").next, next);
        }
    }

    /// #246 UX: Restart sends one command, and only while the helper is
    /// stopped. A frontend cannot restart a helper that runs.
    #[test]
    fn restart_acts_only_while_the_helper_is_stopped() {
        let mut snapshot = Snapshot::new();
        let restart = AdapterCommand::RestartHelper {
            protocol: ProtocolId::WhatsApp,
        };
        for state in [
            HelperState::Idle,
            HelperState::Busy,
            HelperState::Running,
            HelperState::Restarting,
            HelperState::Missing,
        ] {
            helper(&mut snapshot, state);
            snapshot.restart_helper(ProtocolId::WhatsApp);
            assert!(snapshot.take_commands().is_empty(), "{state:?}");
        }
        helper(&mut snapshot, HelperState::Stopped(HelperFault::Crashed));
        snapshot.restart_helper(ProtocolId::Telegram);
        assert!(snapshot.take_commands().is_empty(), "another protocol");
        snapshot.restart_helper(ProtocolId::WhatsApp);
        assert_eq!(snapshot.take_commands(), vec![restart]);
        assert_eq!(snapshot.error, None, "the old error block is gone");
        // The adapter reports the new start.
        helper(&mut snapshot, HelperState::Busy);
        assert_eq!(
            whatsapp_row(&snapshot).helper_label(),
            Some("Helper is busy…")
        );
        helper(&mut snapshot, HelperState::Running);
        assert_eq!(whatsapp_row(&snapshot).helper_label(), None);
    }

    /// #246 UX: a slow helper shows a busy row. It is not an error, and it
    /// changes no link state.
    #[test]
    fn a_busy_or_restarting_helper_is_a_row_state_not_an_error() {
        let mut snapshot = Snapshot::new();
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::WhatsApp,
            state: AccountState::Linked,
        });
        for (state, label) in [
            (HelperState::Busy, "Helper is busy…"),
            (HelperState::Restarting, "Helper stopped. Restarting…"),
        ] {
            helper(&mut snapshot, state);
            assert_eq!(whatsapp_row(&snapshot).helper_label(), Some(label));
            assert_eq!(snapshot.error, None);
            assert!(
                whatsapp_row(&snapshot).linked(),
                "a helper state never unlinks"
            );
        }
    }

    /// ADR 0013 open question: a helper that is gone answers no load. Its
    /// state ends the loading markers of its protocol, and of no other.
    #[test]
    fn a_helper_that_is_gone_ends_the_loads_of_its_protocol() {
        for state in [
            HelperState::Idle,
            HelperState::Restarting,
            HelperState::Stopped(HelperFault::Crashed),
            HelperState::Missing,
        ] {
            let mut snapshot = Snapshot::new();
            let chat = "whatsapp:111@s.whatsapp.net".to_owned();
            snapshot.chat_list_loading.insert(ProtocolId::WhatsApp);
            snapshot
                .history_loading
                .insert((ProtocolId::WhatsApp, chat.clone()));
            snapshot
                .older_loading
                .insert((ProtocolId::WhatsApp, chat.clone(), None));
            snapshot.chat_list_loading.insert(ProtocolId::Telegram);
            helper(&mut snapshot, state);
            assert!(!snapshot.chat_list_loading.contains(&ProtocolId::WhatsApp));
            assert!(snapshot.history_loading.is_empty(), "{state:?}");
            assert!(snapshot.older_loading.is_empty(), "{state:?}");
            assert!(snapshot.chat_list_loading.contains(&ProtocolId::Telegram));
        }
    }

    fn helper_of(snapshot: &mut Snapshot, protocol: ProtocolId, state: HelperState) {
        snapshot.apply(AdapterEvent::Helper { protocol, state });
    }

    fn row_of(snapshot: &Snapshot, protocol: ProtocolId) -> &AccountRow {
        snapshot
            .accounts
            .iter()
            .find(|row| row.caps.id == protocol)
            .expect("account row")
    }

    /// #248 UX: when the Signal helper stops for good, the Signal account
    /// row shows "Helper stopped." with Restart, and the error block names
    /// Signal. The WhatsApp helper is another process: its row does not
    /// change, and Restart on it does nothing.
    #[test]
    fn a_stopped_signal_helper_shows_restart_and_keeps_the_whatsapp_row() {
        let mut snapshot = Snapshot::new();
        helper_of(&mut snapshot, ProtocolId::WhatsApp, HelperState::Running);
        helper_of(&mut snapshot, ProtocolId::Signal, HelperState::Running);
        helper_of(
            &mut snapshot,
            ProtocolId::Signal,
            HelperState::Stopped(HelperFault::Crashed),
        );

        let signal = row_of(&snapshot, ProtocolId::Signal);
        assert!(signal.helper_stopped());
        assert_eq!(signal.helper_label(), Some("Helper stopped."));
        let error = snapshot.error.clone().expect("error block");
        assert_eq!(error.happened, "The Signal helper stopped.");
        assert_eq!(
            error.next,
            "Click Restart on the Signal account. If it stops again, reinstall thinwire."
        );
        let whatsapp = row_of(&snapshot, ProtocolId::WhatsApp);
        assert_eq!(whatsapp.helper, Some(HelperState::Running));
        assert_eq!(whatsapp.helper_label(), None);

        snapshot.restart_helper(ProtocolId::WhatsApp);
        assert!(snapshot.take_commands().is_empty(), "WhatsApp runs");
        snapshot.restart_helper(ProtocolId::Signal);
        assert_eq!(
            snapshot.take_commands(),
            vec![AdapterCommand::RestartHelper {
                protocol: ProtocolId::Signal,
            }]
        );
    }

    /// ADR 0013 decision 6 for Signal: a missing helper keeps the row with
    /// the reason, and the core opens no notice and starts no link.
    #[test]
    fn a_missing_signal_helper_keeps_the_row_and_names_the_reason() {
        let mut snapshot = Snapshot::new();
        helper_of(&mut snapshot, ProtocolId::Signal, HelperState::Missing);
        let row = row_of(&snapshot, ProtocolId::Signal);
        assert!(row.helper_missing());
        assert_eq!(
            row.helper_missing_text(),
            "Signal helper missing. Reinstall thinwire."
        );
        assert!(snapshot.helper_missing(ProtocolId::Signal));
        assert!(!snapshot.helper_missing(ProtocolId::WhatsApp));
        assert_eq!(snapshot.error, None, "no error for a user with no Signal");
    }

    /// A saved acceptance starts the helper and leaves the notice off the
    /// screen. The first link still needs the notice, and a link does not
    /// start from the hidden screen.
    #[cfg(feature = "signal-local")]
    #[test]
    fn a_restored_signal_notice_starts_the_helper_without_showing_the_notice() {
        let mut snapshot = Snapshot::new();
        snapshot.begin_signal_link();
        assert!(
            snapshot.take_commands().is_empty(),
            "the first link needs the notice"
        );
        snapshot.open_signal_notice();
        snapshot.begin_signal_link();
        assert!(
            snapshot.take_commands().is_empty(),
            "an open notice is not an acceptance"
        );

        let mut snapshot = Snapshot::new();
        snapshot.restore_signal_notice();
        assert!(snapshot.signal_notice_acknowledged());
        assert_eq!(snapshot.signal_screen, SignalScreen::Hidden);
        assert_eq!(
            snapshot.take_commands(),
            vec![AdapterCommand::SignalAcknowledgeNotice]
        );
        snapshot.begin_signal_link();
        assert!(
            snapshot.take_commands().is_empty(),
            "a remembered acceptance does not start a link on its own"
        );
    }

    /// Link after a remembered notice must not drop that acceptance.
    /// The account is not linked yet. A keychain failure leaves it there.
    /// A re-show from the link screen still drops the acceptance.
    #[cfg(feature = "signal-local")]
    #[test]
    fn link_after_a_remembered_notice_keeps_the_acceptance() {
        let mut snapshot = Snapshot::new();
        snapshot.restore_signal_notice();
        let _ = snapshot.take_commands();
        snapshot.open_signal_notice();
        assert!(snapshot.signal_notice_acknowledged());
        assert!(
            !snapshot.take_signal_notice_forget(),
            "Link does not ask to clear the saved hash"
        );
        assert_eq!(snapshot.signal_screen, SignalScreen::Link);

        snapshot.open_signal_notice();
        assert!(!snapshot.signal_notice_acknowledged());
        assert!(
            snapshot.take_signal_notice_forget(),
            "a re-show from the link screen still clears the saved hash"
        );
    }

    /// #248: with no Signal helper program the notice does not open. When
    /// the helper goes away while the link screen is open, a link start
    /// does nothing, and Start does not stay disabled with no answer. When
    /// the helper is back, the same screen starts the link.
    #[cfg(feature = "signal-local")]
    #[test]
    fn a_signal_link_does_not_start_while_the_helper_is_missing() {
        let mut snapshot = Snapshot::new();
        helper_of(&mut snapshot, ProtocolId::Signal, HelperState::Missing);
        snapshot.open_signal_notice();
        assert_eq!(snapshot.signal_screen, SignalScreen::Hidden);
        assert!(snapshot.take_commands().is_empty());

        helper_of(&mut snapshot, ProtocolId::Signal, HelperState::Idle);
        snapshot.open_signal_notice();
        snapshot.acknowledge_signal_notice();
        assert_eq!(snapshot.signal_screen, SignalScreen::Link);
        snapshot.take_commands();

        helper_of(&mut snapshot, ProtocolId::Signal, HelperState::Missing);
        snapshot.begin_signal_link();
        assert!(snapshot.take_commands().is_empty(), "no link command");
        assert!(!snapshot.signal_started, "Start is not held");
        assert_eq!(
            snapshot
                .helper_notice(ProtocolId::Signal)
                .map(|notice| notice.label),
            Some("Helper missing"),
            "the link screen shows the reason"
        );

        helper_of(&mut snapshot, ProtocolId::Signal, HelperState::Idle);
        snapshot.begin_signal_link();
        assert!(snapshot.signal_started);
        assert!(matches!(
            snapshot.take_commands().as_slice(),
            [AdapterCommand::SignalBeginLink { .. }]
        ));
    }

    /// #248: the provisioning URL of a Signal helper that is gone is not
    /// valid. While the helper starts again, the link screen drops it and
    /// the link stays. When the helper stopped for good, the link ends, the
    /// screen shows the helper error, and the user can start again.
    #[cfg(feature = "signal-local")]
    #[test]
    fn a_signal_helper_failure_during_linking_drops_the_qr_and_allows_a_new_start() {
        let mut snapshot = Snapshot::new();
        snapshot.open_signal_notice();
        snapshot.acknowledge_signal_notice();
        snapshot.begin_signal_link();
        let commands = snapshot.take_commands();
        let Some(AdapterCommand::SignalBeginLink { generation }) = commands.last() else {
            panic!("no link start: {commands:?}");
        };
        let first = *generation;
        let qr = |generation| AdapterEvent::SignalQr {
            code: thinwire_protocol::RedactedPairingSecret::new("sgnl://one"),
            generation,
        };
        snapshot.apply(qr(first));
        assert_eq!(snapshot.signal_qr.as_deref(), Some("sgnl://one"));

        helper_of(&mut snapshot, ProtocolId::Signal, HelperState::Restarting);
        assert_eq!(snapshot.signal_qr, None, "the old URL is not valid");
        assert!(snapshot.signal_started, "the link stays");
        // The new helper sends a URL for the same link.
        snapshot.apply(qr(first));
        assert_eq!(snapshot.signal_qr.as_deref(), Some("sgnl://one"));

        helper_of(
            &mut snapshot,
            ProtocolId::Signal,
            HelperState::Stopped(HelperFault::Crashed),
        );
        assert_eq!(snapshot.signal_qr, None);
        assert!(!snapshot.signal_started, "Start linking is enabled again");
        assert_eq!(snapshot.signal_screen, SignalScreen::Link);
        let notice = snapshot
            .helper_notice(ProtocolId::Signal)
            .expect("the link screen shows the helper");
        assert_eq!(notice.label, "Helper stopped.");
        assert_eq!(
            notice.error.map(|error| error.happened.as_str()),
            Some("The Signal helper stopped.")
        );
        // A URL of the old link does not come back.
        snapshot.apply(qr(first));
        assert_eq!(snapshot.signal_qr, None);

        snapshot.begin_signal_link();
        let commands = snapshot.take_commands();
        let [AdapterCommand::SignalBeginLink { generation }] = commands.as_slice() else {
            panic!("no new link start: {commands:?}");
        };
        assert_ne!(*generation, first, "a new link, not the old one");
    }

    /// #248: a release build has WhatsApp and Signal. Each has its own
    /// pairing. The end of a WhatsApp pairing, or of the WhatsApp helper,
    /// must not drop the provisioning URL of a Signal link that runs. The
    /// reverse holds too.
    #[cfg(all(feature = "signal-local", feature = "whatsapp-web"))]
    #[test]
    fn the_end_of_one_pairing_keeps_the_pairing_of_the_other_protocol() {
        let signal_qr = |generation| AdapterEvent::SignalQr {
            code: thinwire_protocol::RedactedPairingSecret::new("sgnl://one"),
            generation,
        };
        let whatsapp_qr = |generation| AdapterEvent::WhatsAppQr {
            code: thinwire_protocol::RedactedPairingSecret::new("qr-one"),
            generation,
        };
        let phone = WhatsAppPhoneVault::new();

        // A Signal link runs. The WhatsApp helper stops for good.
        let mut snapshot = Snapshot::new();
        snapshot.open_signal_notice();
        snapshot.acknowledge_signal_notice();
        snapshot.begin_signal_link();
        let commands = snapshot.take_commands();
        let Some(AdapterCommand::SignalBeginLink { generation }) = commands.last() else {
            panic!("no link start: {commands:?}");
        };
        let link = *generation;
        helper_of(
            &mut snapshot,
            ProtocolId::WhatsApp,
            HelperState::Stopped(HelperFault::Crashed),
        );
        assert!(snapshot.signal_started, "the Signal link stays");
        snapshot.apply(signal_qr(link));
        assert_eq!(snapshot.signal_qr.as_deref(), Some("sgnl://one"));

        // A WhatsApp pairing runs. The Signal helper stops for good.
        let mut snapshot = Snapshot::new();
        snapshot.open_whatsapp_risk_gate();
        snapshot.acknowledge_whatsapp_risk();
        snapshot.begin_whatsapp_link(&phone);
        let commands = snapshot.take_commands();
        let Some(AdapterCommand::WhatsAppBeginLink { generation }) = commands.last() else {
            panic!("no pairing start: {commands:?}");
        };
        let pairing = *generation;
        helper_of(
            &mut snapshot,
            ProtocolId::Signal,
            HelperState::Stopped(HelperFault::Crashed),
        );
        assert!(snapshot.whatsapp_started, "the WhatsApp pairing stays");
        snapshot.apply(whatsapp_qr(pairing));
        assert_eq!(snapshot.whatsapp_qr.as_deref(), Some("qr-one"));
    }

    /// A link replaces the pairing line of the status strip. The strip
    /// does not say "pairing requested" for a linked account.
    #[cfg(feature = "whatsapp-web")]
    #[test]
    fn a_whatsapp_link_replaces_the_pairing_status() {
        let phone = thinwire_protocol::WhatsAppPhoneVault::new();
        let mut snapshot = Snapshot::new();
        snapshot.open_whatsapp_risk_gate();
        snapshot.acknowledge_whatsapp_risk();
        snapshot.begin_whatsapp_link(&phone);
        assert_eq!(snapshot.status_line(), WHATSAPP_PAIRING_REQUESTED);
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::WhatsApp,
            state: AccountState::Linked,
        });
        assert_eq!(snapshot.status_line(), WHATSAPP_LINKED);
        assert_eq!(snapshot.whatsapp_screen, WhatsAppScreen::Hidden);

        // The pair screen closed first. The link still replaces the line.
        let mut snapshot = Snapshot::new();
        snapshot.open_whatsapp_risk_gate();
        snapshot.acknowledge_whatsapp_risk();
        snapshot.begin_whatsapp_link(&phone);
        snapshot.whatsapp_screen = WhatsAppScreen::Hidden;
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::WhatsApp,
            state: AccountState::Linked,
        });
        assert_eq!(snapshot.status_line(), WHATSAPP_LINKED);
    }

    /// The saved session opens after the gate, with no pairing. The pair
    /// screen then closes, so its Cancel cannot end that session.
    #[cfg(feature = "whatsapp-web")]
    #[test]
    fn a_saved_session_that_opens_after_the_gate_closes_the_pair_screen() {
        let mut snapshot = Snapshot::new();
        snapshot.open_whatsapp_risk_gate();
        snapshot.acknowledge_whatsapp_risk();
        assert_eq!(snapshot.whatsapp_screen, WhatsAppScreen::Pair);
        assert!(!snapshot.whatsapp_started);
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::WhatsApp,
            state: AccountState::Linked,
        });
        assert_eq!(snapshot.whatsapp_screen, WhatsAppScreen::Hidden);
        assert_eq!(snapshot.status_line(), WHATSAPP_LINKED);
        assert!(whatsapp_row(&snapshot).linked());
    }

    /// ADR 0013 decision 6: a missing helper keeps the account row, with
    /// the reason. It shows no error block at start.
    #[test]
    fn a_missing_helper_keeps_the_row_and_names_the_reason() {
        let mut snapshot = Snapshot::new();
        helper(&mut snapshot, HelperState::Missing);
        let row = whatsapp_row(&snapshot);
        assert!(row.helper_missing());
        assert_eq!(row.helper_label(), Some("Helper missing"));
        assert_eq!(
            row.helper_missing_text(),
            "WhatsApp helper missing. Reinstall thinwire."
        );
        assert!(snapshot.helper_missing(ProtocolId::WhatsApp));
        assert!(!snapshot.helper_missing(ProtocolId::Signal));
        assert_eq!(snapshot.error, None, "no error for a user with no WhatsApp");
    }

    /// The pair screen draws no account row and no error block. It gets the
    /// helper line, and the error of a helper that stopped, from the core.
    #[test]
    fn a_gate_screen_gets_the_helper_line_and_its_error() {
        let mut snapshot = Snapshot::new();
        assert_eq!(snapshot.helper_notice(ProtocolId::WhatsApp), None);
        helper(&mut snapshot, HelperState::Busy);
        let busy = snapshot
            .helper_notice(ProtocolId::WhatsApp)
            .expect("a notice");
        assert_eq!(busy.label, "Helper is busy…");
        assert_eq!(busy.error, None, "busy is not an error");
        helper(&mut snapshot, HelperState::Stopped(HelperFault::Crashed));
        let stopped = snapshot
            .helper_notice(ProtocolId::WhatsApp)
            .expect("a notice");
        assert_eq!(stopped.label, "Helper stopped.");
        assert_eq!(
            stopped.error.map(|error| error.happened.as_str()),
            Some("The WhatsApp helper stopped.")
        );
        assert_eq!(snapshot.helper_notice(ProtocolId::Telegram), None);

        // PR #256 review: an error of another protocol takes the error
        // block. The gate still shows the error of its own helper.
        snapshot.apply(AdapterEvent::CommandFailed {
            protocol: ProtocolId::Telegram,
            conversation_id: None,
            detail: "telegram-detail-fixture".into(),
        });
        assert!(
            snapshot
                .error
                .as_ref()
                .is_some_and(|error| error.why == "telegram-detail-fixture"),
            "the error block now has the Telegram error"
        );
        let stopped = snapshot
            .helper_notice(ProtocolId::WhatsApp)
            .expect("a notice");
        assert_eq!(
            stopped.error.map(|error| error.happened.as_str()),
            Some("The WhatsApp helper stopped.")
        );

        // A new helper state ends the old error.
        helper(&mut snapshot, HelperState::Busy);
        let busy = snapshot
            .helper_notice(ProtocolId::WhatsApp)
            .expect("a notice");
        assert_eq!(busy.error, None);
        helper(&mut snapshot, HelperState::Running);
        assert_eq!(snapshot.helper_notice(ProtocolId::WhatsApp), None);
    }

    /// The QR of a helper that is gone is not valid. While the helper starts
    /// again, the pair screen drops it and the pairing stays. When the helper
    /// stopped for good, the pairing ends and the user can start it again.
    #[cfg(feature = "whatsapp-web")]
    #[test]
    fn a_helper_failure_during_pairing_drops_the_code_and_allows_a_new_start() {
        let phone = WhatsAppPhoneVault::new();
        let mut snapshot = Snapshot::new();
        snapshot.open_whatsapp_risk_gate();
        snapshot.acknowledge_whatsapp_risk();
        snapshot.begin_whatsapp_link(&phone);
        let commands = snapshot.take_commands();
        let Some(AdapterCommand::WhatsAppBeginLink { generation }) = commands.last() else {
            panic!("no pairing start: {commands:?}");
        };
        let qr = |generation| AdapterEvent::WhatsAppQr {
            code: thinwire_protocol::RedactedPairingSecret::new("qr-one"),
            generation,
        };
        snapshot.apply(qr(*generation));
        assert_eq!(snapshot.whatsapp_qr.as_deref(), Some("qr-one"));

        helper(&mut snapshot, HelperState::Restarting);
        assert_eq!(snapshot.whatsapp_qr, None, "the old QR is not valid");
        assert!(snapshot.whatsapp_started, "the pairing stays");
        // The new helper sends a QR for the same pairing.
        snapshot.apply(qr(*generation));
        assert_eq!(snapshot.whatsapp_qr.as_deref(), Some("qr-one"));

        helper(&mut snapshot, HelperState::Stopped(HelperFault::Crashed));
        assert_eq!(snapshot.whatsapp_qr, None);
        assert!(!snapshot.whatsapp_started, "Start pairing is enabled again");
        assert_eq!(snapshot.whatsapp_screen, WhatsAppScreen::Pair);
        snapshot.begin_whatsapp_link(&phone);
        assert!(matches!(
            snapshot.take_commands().as_slice(),
            [AdapterCommand::WhatsAppBeginLink { .. }]
        ));
    }

    /// PR #256 review: the adapter ends a pairing itself (`Linking`, then
    /// `Unlinked`: a pair code that failed, or a ban), and the helper stays
    /// `Running`. The pair screen stays open, the old QR goes, and Start is
    /// enabled again. A new start sends a new pairing. QR codes that ran out
    /// are a different end: the expired screen stays.
    #[cfg(feature = "whatsapp-web")]
    #[test]
    fn a_pairing_that_the_adapter_ended_enables_start_again() {
        let phone = WhatsAppPhoneVault::new();
        let mut snapshot = Snapshot::new();
        snapshot.open_whatsapp_risk_gate();
        snapshot.acknowledge_whatsapp_risk();
        snapshot.begin_whatsapp_link(&phone);
        let commands = snapshot.take_commands();
        let Some(AdapterCommand::WhatsAppBeginLink { generation }) = commands.last() else {
            panic!("no pairing start: {commands:?}");
        };
        let first = *generation;
        helper(&mut snapshot, HelperState::Running);
        let account = |state| AdapterEvent::Account {
            protocol: ProtocolId::WhatsApp,
            state,
        };
        snapshot.apply(account(AccountState::Linking));
        snapshot.apply(AdapterEvent::WhatsAppQr {
            code: thinwire_protocol::RedactedPairingSecret::new("qr-one"),
            generation: first,
        });
        assert_eq!(snapshot.whatsapp_qr.as_deref(), Some("qr-one"));
        assert!(snapshot.whatsapp_started);

        snapshot.apply(account(AccountState::Unlinked));
        assert_eq!(snapshot.whatsapp_qr, None, "the old QR is not valid");
        assert!(!snapshot.whatsapp_started, "Start is enabled again");
        assert_eq!(snapshot.whatsapp_screen, WhatsAppScreen::Pair);
        assert!(snapshot.take_commands().is_empty(), "no command by itself");

        snapshot.begin_whatsapp_link(&phone);
        assert!(snapshot.whatsapp_started);
        let commands = snapshot.take_commands();
        let [AdapterCommand::WhatsAppBeginLink { generation }] = commands.as_slice() else {
            panic!("no new pairing start: {commands:?}");
        };
        assert_ne!(*generation, first, "a new pairing, not the old one");
    }

    /// PR #256 review: a phone text that is too long does not start a
    /// pairing. The pair screen says why, the text stays, and Start stays
    /// enabled. So no pairing stays open with no answer.
    #[cfg(feature = "whatsapp-web")]
    #[test]
    fn a_phone_text_that_is_too_long_does_not_start_a_pairing() {
        let phone = WhatsAppPhoneVault::new();
        let mut snapshot = Snapshot::new();
        snapshot.open_whatsapp_risk_gate();
        snapshot.acknowledge_whatsapp_risk();
        snapshot.take_commands();

        let long = "1".repeat(WHATSAPP_PHONE_MAX_CHARS + 1);
        snapshot.set_whatsapp_phone(long.clone());
        snapshot.begin_whatsapp_link(&phone);
        assert!(snapshot.take_commands().is_empty(), "no pairing command");
        assert!(!snapshot.whatsapp_started, "Start stays enabled");
        assert_eq!(snapshot.whatsapp_phone_note, Some(WHATSAPP_PHONE_TOO_LONG));
        assert_eq!(snapshot.whatsapp_phone, long, "the text stays");
        assert_eq!(phone.phone(), None, "nothing went to the vault");

        // The user corrects the text: the note goes, and the pairing starts.
        snapshot.set_whatsapp_phone("+49 1555 0100".into());
        assert_eq!(snapshot.whatsapp_phone_note, None);
        snapshot.begin_whatsapp_link(&phone);
        assert!(snapshot.whatsapp_started);
        assert_eq!(phone.phone().as_deref(), Some("+49 1555 0100"));
        assert!(matches!(
            snapshot.take_commands().as_slice(),
            [AdapterCommand::WhatsAppBeginLink { .. }]
        ));
    }

    /// PR #256 review: the helper program goes away while the pair screen
    /// is open. A pairing start then does nothing: no command goes out, and
    /// Start does not stay disabled with no answer. When the helper is back,
    /// the same screen starts the pairing.
    #[cfg(feature = "whatsapp-web")]
    #[test]
    fn a_pairing_does_not_start_while_the_helper_is_missing() {
        let phone = WhatsAppPhoneVault::new();
        let mut snapshot = Snapshot::new();
        snapshot.open_whatsapp_risk_gate();
        snapshot.acknowledge_whatsapp_risk();
        snapshot.take_commands();

        helper(&mut snapshot, HelperState::Missing);
        assert_eq!(snapshot.whatsapp_screen, WhatsAppScreen::Pair);
        snapshot.set_whatsapp_phone("15550100".into());
        snapshot.begin_whatsapp_link(&phone);
        assert!(snapshot.take_commands().is_empty(), "no pairing command");
        assert!(!snapshot.whatsapp_started, "Start is not held");
        assert_eq!(phone.phone(), None, "the phone stays out of the vault");
        assert_eq!(
            snapshot
                .helper_notice(ProtocolId::WhatsApp)
                .map(|notice| notice.label),
            Some("Helper missing"),
            "the pair screen shows the reason"
        );

        helper(&mut snapshot, HelperState::Idle);
        snapshot.begin_whatsapp_link(&phone);
        assert!(snapshot.whatsapp_started);
        assert!(matches!(
            snapshot.take_commands().as_slice(),
            [AdapterCommand::WhatsAppBeginLink { .. }]
        ));
    }

    /// Pairing QR codes run out (`PairingQrCodesExhausted`). The payload
    /// leaves the snapshot. A new code starts a new pairing and keeps the
    /// phone already in the vault. A second start while the code is valid
    /// does nothing.
    #[cfg(feature = "whatsapp-web")]
    #[test]
    fn an_exhausted_qr_drops_the_payload_and_a_new_code_starts_again() {
        use thinwire_protocol::WHATSAPP_QR_EXHAUSTED;

        let phone = WhatsAppPhoneVault::new();
        let mut snapshot = Snapshot::new();
        snapshot.open_whatsapp_risk_gate();
        snapshot.acknowledge_whatsapp_risk();
        snapshot.set_whatsapp_phone("15550100".into());
        snapshot.begin_whatsapp_link(&phone);
        let commands = snapshot.take_commands();
        let Some(AdapterCommand::WhatsAppBeginLink { generation }) = commands.last() else {
            panic!("no pairing start: {commands:?}");
        };
        let first = *generation;
        assert_eq!(phone.phone().as_deref(), Some("15550100"));
        let payload = "2@one-line\n2@same-payload";
        snapshot.apply(AdapterEvent::WhatsAppQr {
            code: thinwire_protocol::RedactedPairingSecret::new(payload),
            generation: first,
        });
        assert_eq!(snapshot.whatsapp_qr.as_deref(), Some(payload));
        assert!(
            !format!("{snapshot:?}").contains(payload),
            "debug output has no payload"
        );
        assert!(!snapshot.whatsapp_qr_exhausted);

        snapshot.begin_whatsapp_link(&phone);
        assert!(
            snapshot.take_commands().is_empty(),
            "a second start while the code is valid does nothing"
        );

        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::WhatsApp,
            status: AdapterStatus::Error,
            detail: "some other failure".into(),
        });
        assert!(!snapshot.whatsapp_qr_exhausted);
        assert_eq!(snapshot.whatsapp_qr.as_deref(), Some(payload));

        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::WhatsApp,
            status: AdapterStatus::Error,
            detail: WHATSAPP_QR_EXHAUSTED.into(),
        });
        assert!(snapshot.whatsapp_qr_exhausted);
        assert_eq!(
            snapshot.whatsapp_qr, None,
            "the expired payload is not shown"
        );
        assert!(snapshot.whatsapp_started, "the pair screen stays open");
        assert!(!format!("{snapshot:?}").contains(payload));

        snapshot.begin_whatsapp_link(&phone);
        assert!(!snapshot.whatsapp_qr_exhausted);
        assert_eq!(
            phone.phone().as_deref(),
            Some("15550100"),
            "the vault keeps the number"
        );
        let commands = snapshot.take_commands();
        let [AdapterCommand::WhatsAppBeginLink { generation }] = commands.as_slice() else {
            panic!("no new pairing: {commands:?}");
        };
        assert_ne!(*generation, first, "the new code is a new pairing");
        snapshot.apply(AdapterEvent::WhatsAppQr {
            code: thinwire_protocol::RedactedPairingSecret::new("2@old"),
            generation: first,
        });
        assert_eq!(
            snapshot.whatsapp_qr, None,
            "the old pairing does not return"
        );
        snapshot.apply(AdapterEvent::WhatsAppQr {
            code: thinwire_protocol::RedactedPairingSecret::new("2@new"),
            generation: *generation,
        });
        assert_eq!(snapshot.whatsapp_qr.as_deref(), Some("2@new"));
        assert!(!snapshot.whatsapp_qr_exhausted);
    }

    /// The helper sends `Status` for used-up QR codes, then `Account`
    /// `Unlinked`, while the row is `Linking`. The expired screen stays.
    /// Get new code starts a new pairing and keeps the vault number.
    #[cfg(feature = "whatsapp-web")]
    #[test]
    fn an_unlinked_account_keeps_the_expired_qr_screen() {
        use thinwire_protocol::WHATSAPP_QR_EXHAUSTED;

        let phone = WhatsAppPhoneVault::new();
        let mut snapshot = Snapshot::new();
        snapshot.open_whatsapp_risk_gate();
        snapshot.acknowledge_whatsapp_risk();
        snapshot.set_whatsapp_phone("15550100".into());
        snapshot.begin_whatsapp_link(&phone);
        let commands = snapshot.take_commands();
        let Some(AdapterCommand::WhatsAppBeginLink { generation }) = commands.last() else {
            panic!("no pairing start: {commands:?}");
        };
        let first = *generation;
        let account = |state| AdapterEvent::Account {
            protocol: ProtocolId::WhatsApp,
            state,
        };
        snapshot.apply(account(AccountState::Linking));
        let payload = "2@one-line\n2@same-payload";
        snapshot.apply(AdapterEvent::WhatsAppQr {
            code: thinwire_protocol::RedactedPairingSecret::new(payload),
            generation: first,
        });

        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::WhatsApp,
            status: AdapterStatus::Error,
            detail: WHATSAPP_QR_EXHAUSTED.into(),
        });
        snapshot.apply(account(AccountState::Unlinked));

        assert!(snapshot.whatsapp_qr_exhausted, "the expired screen stays");
        assert!(
            snapshot.whatsapp_started,
            "Get new code does not start from an empty field"
        );
        assert_eq!(snapshot.whatsapp_qr, None, "the payload is dropped");
        assert_eq!(snapshot.whatsapp_screen, WhatsAppScreen::Pair);
        assert!(snapshot.whatsapp_phone.is_empty());
        assert_eq!(phone.phone().as_deref(), Some("15550100"));
        assert!(!format!("{snapshot:?}").contains(payload));

        snapshot.begin_whatsapp_link(&phone);
        assert!(!snapshot.whatsapp_qr_exhausted);
        assert_eq!(
            phone.phone().as_deref(),
            Some("15550100"),
            "the vault keeps the number"
        );
        let commands = snapshot.take_commands();
        let [AdapterCommand::WhatsAppBeginLink { generation }] = commands.as_slice() else {
            panic!("no new pairing: {commands:?}");
        };
        assert_ne!(*generation, first, "Get new code is a new pairing");
        snapshot.apply(AdapterEvent::WhatsAppQr {
            code: thinwire_protocol::RedactedPairingSecret::new("2@old"),
            generation: first,
        });
        assert_eq!(
            snapshot.whatsapp_qr, None,
            "the old pairing does not return"
        );
    }

    /// The helper stops while the codes are used up. The expired screen
    /// stays, and Get new code does not write the empty phone field over
    /// the vault number.
    #[cfg(feature = "whatsapp-web")]
    #[test]
    fn a_stopped_helper_keeps_the_expired_qr_screen_and_the_phone() {
        use thinwire_protocol::WHATSAPP_QR_EXHAUSTED;

        let phone = WhatsAppPhoneVault::new();
        let mut snapshot = Snapshot::new();
        snapshot.open_whatsapp_risk_gate();
        snapshot.acknowledge_whatsapp_risk();
        snapshot.set_whatsapp_phone("15550100".into());
        snapshot.begin_whatsapp_link(&phone);
        snapshot.take_commands();
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::WhatsApp,
            state: AccountState::Linking,
        });
        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::WhatsApp,
            status: AdapterStatus::Error,
            detail: WHATSAPP_QR_EXHAUSTED.into(),
        });
        assert!(snapshot.whatsapp_qr_exhausted);
        assert!(snapshot.whatsapp_started);
        assert!(snapshot.whatsapp_phone.is_empty());

        helper(&mut snapshot, HelperState::Stopped(HelperFault::Crashed));
        assert!(
            snapshot.whatsapp_qr_exhausted,
            "the expired screen stays after the helper stops"
        );
        assert!(
            snapshot.whatsapp_started,
            "Start stays off, so the empty field is not sent"
        );
        assert_eq!(snapshot.whatsapp_qr, None);
        assert_eq!(snapshot.whatsapp_screen, WhatsAppScreen::Pair);
        assert_eq!(phone.phone().as_deref(), Some("15550100"));

        snapshot.begin_whatsapp_link(&phone);
        assert!(!snapshot.whatsapp_qr_exhausted);
        assert_eq!(
            phone.phone().as_deref(),
            Some("15550100"),
            "Get new code keeps the vault number"
        );
        assert!(matches!(
            snapshot.take_commands().as_slice(),
            [AdapterCommand::WhatsAppBeginLink { .. }]
        ));
    }

    /// ADR 0013 decision 6: with no helper program, the WhatsApp entry is
    /// disabled. The core refuses the gate too, so no frontend can open it.
    #[cfg(feature = "whatsapp-web")]
    #[test]
    fn the_whatsapp_gate_stays_closed_while_the_helper_is_missing() {
        let mut snapshot = Snapshot::new();
        helper(&mut snapshot, HelperState::Missing);
        snapshot.open_whatsapp_risk_gate();
        assert_eq!(snapshot.whatsapp_screen, WhatsAppScreen::Hidden);
        assert!(snapshot.take_commands().is_empty());
        // The helper is back (a reinstall while the app runs).
        helper(&mut snapshot, HelperState::Idle);
        snapshot.open_whatsapp_risk_gate();
        assert_eq!(snapshot.whatsapp_screen, WhatsAppScreen::RiskGate);
    }

    // endregion: helper process (ADR 0013)

    fn resume_commands(snapshot: &mut Snapshot) -> usize {
        snapshot
            .take_commands()
            .iter()
            .filter(|command| {
                matches!(
                    command,
                    AdapterCommand::TelegramAuth {
                        step: TelegramAuthStep::ApiCredentials,
                        epoch: 0
                    }
                )
            })
            .count()
    }

    #[test]
    fn saved_session_resumes_once_without_first_run() {
        let store = SecretStore::memory();
        seed_override(&store);
        store
            .set(SecretKey::Session, "tdlib-ready")
            .expect("marker");
        let mut snapshot = Snapshot::new();
        snapshot.try_resume(&store, true);
        snapshot.try_resume(&store, true);
        assert_eq!(
            snapshot.center_view(),
            CenterView::Resuming { connecting: true }
        );
        assert_eq!(snapshot.status_text, RESUME_CONNECTING);
        assert_eq!(snapshot.auth, AuthScreen::Idle);
        let commands = snapshot.take_commands();
        assert_eq!(commands.len(), 1, "{commands:?}");
        assert!(matches!(
            commands[0],
            AdapterCommand::TelegramAuth {
                step: TelegramAuthStep::ApiCredentials,
                epoch: 0
            }
        ));
        let debug = format!("{commands:?}");
        assert!(!debug.contains("11111"));
        assert!(!debug.contains("hash-value"));
        assert!(!debug.contains("tdlib-ready"));

        snapshot.apply(AdapterEvent::TelegramAuth {
            phase: TelegramAuthPhase::Ready,
        });
        assert_eq!(snapshot.center_view(), CenterView::Thread);
        assert!(snapshot.has_primary_account());
        assert_eq!(snapshot.auth, AuthScreen::Idle);
        assert_eq!(snapshot.auth_notice, None);
    }

    #[test]
    fn no_saved_session_keeps_first_run() {
        let store = SecretStore::memory();
        seed_override(&store);
        let mut snapshot = Snapshot::new();
        snapshot.try_resume(&store, true);
        assert_eq!(snapshot.center_view(), CenterView::FirstRun);
        assert_eq!(resume_commands(&mut snapshot), 0);
    }

    #[test]
    fn saved_session_without_api_credentials_or_tdlib_keeps_first_run() {
        let store = SecretStore::memory();
        store
            .set(SecretKey::Session, "tdlib-ready")
            .expect("marker");
        let mut snapshot = Snapshot::with_api_source(TelegramApiSource::empty());
        snapshot.try_resume(&store, true);
        assert_eq!(snapshot.center_view(), CenterView::FirstRun);
        assert_eq!(resume_commands(&mut snapshot), 0);

        seed_override(&store);
        let mut feature_off = Snapshot::new();
        feature_off.try_resume(&store, false);
        assert_eq!(feature_off.center_view(), CenterView::FirstRun);
        assert_eq!(resume_commands(&mut feature_off), 0);
    }

    #[test]
    fn resume_waits_for_the_keychain_read_then_arms() {
        let store = SecretStore::detached_for_test();
        let mut snapshot = Snapshot::new();
        snapshot.try_resume(&store, true);
        assert_eq!(resume_commands(&mut snapshot), 0);
        assert_ne!(snapshot.center_view(), CenterView::Thread);
        store.complete_ready_attach_for_test(&[
            (SecretKey::ApiId, "11111"),
            (SecretKey::ApiHash, "hash-value"),
            (SecretKey::Session, "tdlib-ready"),
        ]);
        snapshot.try_resume(&store, true);
        assert_eq!(resume_commands(&mut snapshot), 1);
        assert_eq!(
            snapshot.center_view(),
            CenterView::Resuming { connecting: true }
        );
    }

    #[test]
    fn ended_session_shows_the_phone_step_with_a_notice() {
        let store = SecretStore::memory();
        seed_override(&store);
        store
            .set(SecretKey::Session, "tdlib-ready")
            .expect("marker");
        let mut snapshot = Snapshot::new();
        snapshot.try_resume(&store, true);
        snapshot.apply(AdapterEvent::TelegramAuth {
            phase: TelegramAuthPhase::NeedPhone,
        });
        assert_eq!(snapshot.center_view(), CenterView::Auth);
        assert_eq!(snapshot.auth, AuthScreen::TelegramPhone);
        assert_eq!(snapshot.auth_notice.as_deref(), Some(SESSION_ENDED_NOTICE));
        assert_eq!(snapshot.status_text, SESSION_ENDED_NOTICE);

        snapshot.telegram_phone = "+15551234567".into();
        submit_and_apply(&mut snapshot, &store, TelegramAuthPhase::NeedCode);
        assert_eq!(snapshot.auth_notice, None);
    }

    /// The keychain lost the session marker, but the TDLib database and its
    /// key are there. The app resumes with no click.
    fn unmarked_store() -> SecretStore {
        let store = SecretStore::memory();
        seed_override(&store);
        store
            .set(SecretKey::DbEncryption, "db-key")
            .expect("database key");
        store.set_tdlib_database_for_test(true);
        store
    }

    #[test]
    fn a_tdlib_database_without_the_marker_resumes() {
        let store = unmarked_store();
        let mut snapshot = Snapshot::new();
        snapshot.try_resume(&store, true);
        assert_eq!(
            snapshot.center_view(),
            CenterView::Resuming { connecting: true }
        );
        assert_eq!(resume_commands(&mut snapshot), 1);
        snapshot.apply(AdapterEvent::TelegramAuth {
            phase: TelegramAuthPhase::Ready,
        });
        assert_eq!(snapshot.auth, AuthScreen::Idle);
        assert_eq!(snapshot.auth_notice, None);
    }

    #[test]
    fn a_database_without_its_key_or_a_key_without_a_database_does_not_resume() {
        let no_key = SecretStore::memory();
        seed_override(&no_key);
        no_key.set_tdlib_database_for_test(true);
        let mut snapshot = Snapshot::new();
        snapshot.try_resume(&no_key, true);
        assert_eq!(snapshot.center_view(), CenterView::FirstRun);
        assert_eq!(resume_commands(&mut snapshot), 0);

        let no_database = unmarked_store();
        no_database.set_tdlib_database_for_test(false);
        let mut snapshot = Snapshot::new();
        snapshot.try_resume(&no_database, true);
        assert_eq!(snapshot.center_view(), CenterView::FirstRun);
        assert_eq!(resume_commands(&mut snapshot), 0);
    }

    /// A database with no session (a login that did not end) closes with no
    /// screen, no "session ended" notice, and keeps the mutes.
    #[test]
    fn an_unfinished_login_in_the_database_closes_with_no_screen() {
        for phase in [
            TelegramAuthPhase::NeedPhone,
            TelegramAuthPhase::NeedCode,
            TelegramAuthPhase::NeedTwoFactor,
        ] {
            let store = unmarked_store();
            let mut snapshot = Snapshot::new();
            snapshot.try_resume(&store, true);
            snapshot.take_commands();
            snapshot.apply(AdapterEvent::TelegramAuth { phase });
            assert_eq!(snapshot.auth, AuthScreen::Idle, "{phase:?}");
            assert_eq!(snapshot.center_view(), CenterView::FirstRun, "{phase:?}");
            assert_eq!(snapshot.auth_notice, None, "{phase:?}");
            assert_eq!(snapshot.status_text, SIGN_IN_PROMPT, "{phase:?}");
            assert!(snapshot.ended_accounts.is_empty(), "{phase:?}");
            let commands = snapshot.take_commands();
            assert!(
                commands.iter().any(|command| matches!(
                    command,
                    AdapterCommand::Disconnect {
                        protocol: ProtocolId::Telegram
                    }
                )),
                "{phase:?}: {commands:?}"
            );
        }

        // A marked session that ends still shows the phone step.
        let store = unmarked_store();
        store
            .set(SecretKey::Session, "tdlib-ready")
            .expect("marker");
        let mut snapshot = Snapshot::new();
        snapshot.try_resume(&store, true);
        snapshot.apply(AdapterEvent::TelegramAuth {
            phase: TelegramAuthPhase::NeedPhone,
        });
        assert_eq!(snapshot.auth, AuthScreen::TelegramPhone);
        assert_eq!(snapshot.auth_notice.as_deref(), Some(SESSION_ENDED_NOTICE));
    }

    #[test]
    fn resume_error_falls_back_to_first_run() {
        let store = SecretStore::memory();
        seed_override(&store);
        store
            .set(SecretKey::Session, "tdlib-ready")
            .expect("marker");
        let mut snapshot = Snapshot::new();
        snapshot.try_resume(&store, true);
        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Telegram,
            status: AdapterStatus::Error,
            detail: "TDLib worker is not running. Cancel and try again.".into(),
        });
        assert_eq!(snapshot.center_view(), CenterView::FirstRun);
        snapshot.try_resume(&store, true);
        assert_eq!(resume_commands(&mut snapshot), 1, "only the first try");
    }

    fn telegram_text(chat: i64, id: i64, body: &str) -> ChatMessage {
        ChatMessage {
            protocol: ProtocolId::Telegram,
            conversation_id: format!("telegram:{chat}"),
            id: format!("telegram:{chat}:{id}"),
            sender: "Ada".into(),
            body: body.into(),
            outbound: false,
            delivery: Delivery::Sent,
            sent_at: 0,
            arrival: thinwire_protocol::Arrival::History,
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

    #[test]
    fn a_photo_download_is_queued_once_until_it_fails() {
        use thinwire_protocol::{MessagePhoto, PhotoSize};

        let mut snapshot = Snapshot::new();
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Telegram,
            state: AccountState::Linked,
        });
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: Conversation {
                protocol: ProtocolId::Telegram,
                id: "telegram:1".into(),
                title: "Ada".into(),
                participant: "Ada".into(),
                preview: String::new(),
                unread: 0,
                order: 1,
                last_at: 1,
                is_group: false,
                writable: true,
                placeholder: false,
                muted: false,
                has_topics: false,
                avatar: Default::default(),
            },
        });
        snapshot.select_conversation("telegram:1".into());
        snapshot.take_commands();
        let mut message = telegram_text(1, 2, "");
        message.photo = Some(MessagePhoto {
            width: 100,
            height: 80,
            mini: None,
            sizes: vec![PhotoSize {
                width: 100,
                height: 80,
                file_id: Some(7),
                path: None,
                bytes: None,
            }],
            has_spoiler: false,
            is_secret: false,
            failed: false,
        });
        snapshot.apply(AdapterEvent::MessageReceived { message });
        let chat = "telegram:1";
        let id = "telegram:1:2";
        snapshot.load_photo(ProtocolId::Telegram, chat, id, 7);
        snapshot.load_photo(ProtocolId::Telegram, chat, id, 7);
        let downloads = |commands: &[AdapterCommand]| {
            commands
                .iter()
                .filter(|command| {
                    matches!(command, AdapterCommand::DownloadFile { file_id: 7, .. })
                })
                .count()
        };
        assert_eq!(downloads(&snapshot.take_commands()), 1);
        snapshot.apply(AdapterEvent::PhotoFailed {
            protocol: ProtocolId::Telegram,
            file_id: 7,
        });
        snapshot.load_photo(ProtocolId::Telegram, chat, id, 7);
        assert_eq!(downloads(&snapshot.take_commands()), 1);
    }

    /// A failed thumbnail stays failed. The bubble asks again only from Retry.
    #[test]
    fn a_failed_document_preview_does_not_download_again() {
        use thinwire_protocol::{MessageFile, MessagePhoto, PhotoSize};

        let mut snapshot = telegram_chat_with_rows(&[(2, false)]);
        let mut message = telegram_text(1, 8, "");
        message.file = Some(MessageFile::new(
            "notes.pdf",
            "application/pdf",
            2048,
            11,
            None,
            Some(MessagePhoto {
                width: 40,
                height: 52,
                mini: None,
                sizes: vec![PhotoSize {
                    width: 40,
                    height: 52,
                    file_id: Some(32),
                    path: None,
                    bytes: None,
                }],
                has_spoiler: false,
                is_secret: false,
                failed: false,
            }),
        ));
        snapshot.apply(AdapterEvent::MessageReceived { message });
        let chat = "telegram:1";
        let id = "telegram:1:8";
        snapshot.load_photo(ProtocolId::Telegram, chat, id, 32);
        let downloads = |commands: &[AdapterCommand]| {
            commands
                .iter()
                .filter(|command| {
                    matches!(command, AdapterCommand::DownloadFile { file_id: 32, .. })
                })
                .count()
        };
        assert_eq!(downloads(&snapshot.take_commands()), 1);
        snapshot.apply(AdapterEvent::PhotoFailed {
            protocol: ProtocolId::Telegram,
            file_id: 32,
        });
        let failed = snapshot
            .selected_messages()
            .iter()
            .find(|row| row.id == id)
            .and_then(|row| row.file.as_ref())
            .and_then(|file| file.preview())
            .is_some_and(|preview| preview.failed);
        assert!(failed, "the preview shows the failure");
        // The next frame asks only while the preview is still loading.
        if !failed {
            snapshot.load_photo(ProtocolId::Telegram, chat, id, 32);
        }
        assert_eq!(
            downloads(&snapshot.take_commands()),
            0,
            "a failed preview does not download again"
        );
    }

    /// A PDF preview is the thumbnail already on that message.
    #[test]
    fn a_document_downloads_once_and_a_pdf_preview_stays() {
        use std::path::Path;

        use thinwire_protocol::{AudioPath, ImageBytes, MessageFile, MessagePhoto, PhotoSize};

        let mut snapshot = telegram_chat_with_rows(&[(2, false)]);
        let preview = MessagePhoto {
            width: 40,
            height: 52,
            mini: Some(ImageBytes::new(vec![0xff, 0xd8, 0xff])),
            sizes: vec![PhotoSize {
                width: 40,
                height: 52,
                file_id: Some(32),
                path: None,
                bytes: None,
            }],
            has_spoiler: false,
            is_secret: false,
            failed: false,
        };
        let mut message = telegram_text(1, 3, "");
        message.file = Some(MessageFile::new(
            "notes.pdf",
            "application/pdf",
            2048,
            11,
            None,
            Some(preview),
        ));
        snapshot.apply(AdapterEvent::MessageReceived { message });
        let chat = "telegram:1";
        let id = "telegram:1:3";
        {
            let file = snapshot
                .selected_messages()
                .iter()
                .find(|row| row.id == id)
                .and_then(|row| row.file.as_ref())
                .expect("pdf");
            assert_eq!(file.name(), "notes.pdf");
            assert_eq!(file.kind_label(), "PDF");
            assert_eq!(file.size_label().as_deref(), Some("2 KB"));
            assert_eq!(
                file.preview()
                    .and_then(|photo| photo.mini.as_ref())
                    .map(ImageBytes::as_slice),
                Some([0xff, 0xd8, 0xff].as_slice())
            );
            assert!(
                file.preview()
                    .is_some_and(|photo| photo.sizes.iter().any(|size| size.file_id == Some(32)))
            );
        }
        snapshot.load_photo(ProtocolId::Telegram, chat, id, 32);
        assert!(
            snapshot
                .take_commands()
                .iter()
                .any(|command| matches!(command, AdapterCommand::DownloadFile { file_id: 32, .. })),
            "the pdf thumbnail was not requested"
        );
        snapshot.apply(AdapterEvent::PhotoReady {
            protocol: ProtocolId::Telegram,
            file_id: 32,
            file: thinwire_protocol::PhotoFile::from_path(thinwire_protocol::ImagePath::new(
                "/tmp/hidden-preview.jpg",
            )),
        });
        {
            let preview = snapshot
                .selected_messages()
                .iter()
                .find(|row| row.id == id)
                .and_then(|row| row.file.as_ref())
                .and_then(|file| file.preview())
                .expect("preview");
            assert_eq!(
                preview.sizes[0].path.as_ref().map(|path| path.as_str()),
                Some("/tmp/hidden-preview.jpg")
            );
            assert!(!format!("{preview:?}").contains("hidden-preview"));
        }

        snapshot.download_document(ProtocolId::Telegram, chat, id, 11);
        snapshot.download_document(ProtocolId::Telegram, chat, id, 11);
        let downloads = |commands: &[AdapterCommand]| {
            commands
                .iter()
                .filter(|command| {
                    matches!(
                        command,
                        AdapterCommand::DownloadDocument { file_id: 11, .. }
                    )
                })
                .count()
        };
        assert_eq!(downloads(&snapshot.take_commands()), 1);

        snapshot.apply(AdapterEvent::FileReady {
            protocol: ProtocolId::Telegram,
            file_id: 11,
            path: AudioPath::new("/tmp/hidden-document.bin").expect("path"),
        });
        let preview = {
            let file = snapshot
                .selected_messages()
                .iter()
                .find(|row| row.id == id)
                .and_then(|row| row.file.as_ref())
                .expect("saved pdf");
            assert_eq!(
                file.file().expect("path").as_path(),
                Path::new("/tmp/hidden-document.bin")
            );
            assert!(!format!("{file:?}").contains("hidden-document"));
            file.preview().cloned()
        };
        snapshot.download_document(ProtocolId::Telegram, chat, id, 11);
        assert_eq!(downloads(&snapshot.take_commands()), 0, "already saved");

        let mut refresh = telegram_text(1, 3, "");
        refresh.file = Some(MessageFile::new(
            "notes.pdf",
            "application/pdf",
            2048,
            11,
            None,
            preview,
        ));
        snapshot.apply(AdapterEvent::MessageReceived { message: refresh });
        {
            let kept = snapshot
                .selected_messages()
                .iter()
                .find(|row| row.id == id)
                .and_then(|row| row.file.as_ref())
                .expect("refreshed pdf");
            assert_eq!(
                kept.file().map(AudioPath::as_path),
                Some(Path::new("/tmp/hidden-document.bin"))
            );
            assert!(kept.preview().is_some());
        }

        let mut plain = telegram_text(1, 4, "");
        plain.file = Some(MessageFile::new(
            "notes.txt",
            "text/plain",
            12,
            12,
            None,
            None,
        ));
        snapshot.apply(AdapterEvent::MessageReceived { message: plain });
        let plain_id = "telegram:1:4";
        {
            let plain_file = snapshot
                .selected_messages()
                .iter()
                .find(|row| row.id == plain_id)
                .and_then(|row| row.file.as_ref())
                .expect("plain file");
            assert!(plain_file.preview().is_none());
            assert_eq!(plain_file.name(), "notes.txt");
        }
        snapshot.download_document(ProtocolId::Telegram, chat, plain_id, 12);
        assert!(snapshot.take_commands().iter().any(|command| {
            matches!(
                command,
                AdapterCommand::DownloadDocument { file_id: 12, .. }
            )
        }));
    }

    /// A chat picture sets the picture of its row only. A later update of
    /// the row with no picture keeps it.
    #[test]
    fn a_chat_picture_stays_on_its_row() {
        let mut snapshot = Snapshot::new();
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Telegram,
            state: AccountState::Linked,
        });
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: test_support::telegram_chat(1, "Ada", 1),
        });
        snapshot.apply(AdapterEvent::ChatAvatar {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            bytes: thinwire_protocol::ImageBytes::new(vec![1, 2, 3]),
        });
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: test_support::telegram_chat(1, "Ada Park", 2),
        });
        let row = snapshot.conversations[&ProtocolId::Telegram]
            .iter()
            .find(|row| row.id == "telegram:1")
            .expect("row");
        assert_eq!(row.title, "Ada Park");
        assert_eq!(
            row.avatar
                .bytes
                .as_ref()
                .map(thinwire_protocol::ImageBytes::as_slice),
            Some([1, 2, 3].as_slice())
        );
    }

    fn telegram_chat_with_rows(rows: &[(i64, bool)]) -> Snapshot {
        let mut snapshot = Snapshot::new();
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Telegram,
            state: AccountState::Linked,
        });
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: test_support::telegram_chat(1, "Ada", 1),
        });
        snapshot.select_conversation("telegram:1".into());
        for &(id, outbound) in rows {
            let mut message = telegram_text(1, id, "Hi");
            message.outbound = outbound;
            snapshot.apply(AdapterEvent::MessageReceived { message });
        }
        snapshot.take_commands();
        snapshot
    }

    /// Reply binds the next text send of the same thread, once. Another
    /// open chat does not show it or use it.
    #[test]
    fn a_reply_goes_with_the_next_send_of_its_thread() {
        let mut snapshot = telegram_chat_with_rows(&[(5, false)]);
        snapshot.begin_reply(ProtocolId::Telegram, "telegram:1", "telegram:1:9");
        assert!(snapshot.reply_target().is_none(), "no such message");
        snapshot.begin_reply(ProtocolId::Telegram, "telegram:1", "telegram:1:5");
        assert_eq!(
            snapshot.reply_target().map(|message| message.id.as_str()),
            Some("telegram:1:5")
        );
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: test_support::telegram_chat(2, "Bob", 1),
        });
        snapshot.select_conversation("telegram:2".into());
        assert!(snapshot.reply_target().is_none(), "another chat is open");
        snapshot.select_conversation("telegram:1".into());
        assert!(
            snapshot.reply_target().is_some(),
            "the reply stays with its chat"
        );

        snapshot.compose = "Yes".into();
        snapshot.send_compose();
        let replies: Vec<_> = snapshot
            .take_commands()
            .into_iter()
            .filter_map(|command| match command {
                AdapterCommand::SendText { reply_to, .. } => Some(reply_to),
                _ => None,
            })
            .collect();
        assert_eq!(replies, vec![Some("telegram:1:5".to_string())]);
        assert!(snapshot.reply_target().is_none(), "one send only");
    }

    /// Delete for everyone needs an own message. A reaction shows at once
    /// and the adapter count replaces it.
    #[test]
    fn delete_and_react_check_the_message_and_the_protocol() {
        let mut snapshot = telegram_chat_with_rows(&[(5, false), (6, true)]);
        snapshot.delete_message(ProtocolId::Telegram, "telegram:1", "telegram:1:5", true);
        assert!(snapshot.take_commands().is_empty(), "not an own message");
        snapshot.delete_message(ProtocolId::Telegram, "telegram:1", "telegram:1:6", true);
        assert_eq!(
            snapshot.take_commands(),
            vec![AdapterCommand::DeleteMessage {
                protocol: ProtocolId::Telegram,
                conversation_id: "telegram:1".into(),
                message_id: "telegram:1:6".into(),
                for_everyone: true,
            }]
        );
        snapshot.delete_message(ProtocolId::Telegram, "telegram:1", "telegram:1:5", false);
        assert_eq!(snapshot.take_commands().len(), 1, "for me");

        let reactions = |snapshot: &Snapshot| {
            snapshot
                .selected_messages()
                .iter()
                .find(|message| message.id == "telegram:1:5")
                .map(|message| message.reactions.clone())
                .expect("row")
        };
        snapshot.react(
            ProtocolId::Telegram,
            "telegram:1",
            "telegram:1:5",
            "👍",
            true,
        );
        assert_eq!(
            reactions(&snapshot),
            vec![thinwire_protocol::Reaction {
                emoji: "👍".into(),
                count: 1,
                mine: true
            }]
        );
        assert!(matches!(
            snapshot.take_commands().as_slice(),
            [AdapterCommand::React { add: true, .. }]
        ));
        snapshot.apply(AdapterEvent::MessageReactions {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            message_id: "telegram:1:5".into(),
            reactions: Vec::new(),
        });
        assert!(reactions(&snapshot).is_empty(), "the adapter count wins");

        snapshot.react(
            ProtocolId::Telegram,
            "telegram:1",
            "telegram:1:5",
            &"👍".repeat(20),
            true,
        );
        assert!(snapshot.take_commands().is_empty(), "too long for an emoji");
        for row in &mut snapshot.accounts {
            row.caps.message_actions = thinwire_protocol::MessageActions::NONE;
        }
        snapshot.react(
            ProtocolId::Telegram,
            "telegram:1",
            "telegram:1:5",
            "👍",
            true,
        );
        snapshot.delete_message(ProtocolId::Telegram, "telegram:1", "telegram:1:6", false);
        snapshot.begin_reply(ProtocolId::Telegram, "telegram:1", "telegram:1:5");
        snapshot.begin_edit(ProtocolId::Telegram, "telegram:1", "telegram:1:6");
        assert!(snapshot.take_commands().is_empty(), "no message actions");
        assert!(snapshot.reply_target().is_none());
        assert!(snapshot.edit_target().is_none());
    }

    /// Edit puts the text of an own message in the compose field. Enter
    /// sends the new text once, and the draft of the thread comes back.
    #[test]
    fn an_edit_sends_the_new_text_and_brings_the_draft_back() {
        let mut snapshot = telegram_chat_with_rows(&[(5, false), (6, true)]);
        snapshot.compose = "draft".into();
        snapshot.begin_edit(ProtocolId::Telegram, "telegram:1", "telegram:1:5");
        assert!(snapshot.edit_target().is_none(), "not an own message");
        assert_eq!(snapshot.compose, "draft");

        snapshot.begin_reply(ProtocolId::Telegram, "telegram:1", "telegram:1:5");
        snapshot.begin_edit(ProtocolId::Telegram, "telegram:1", "telegram:1:6");
        assert_eq!(
            snapshot.edit_target().map(|message| message.id.as_str()),
            Some("telegram:1:6")
        );
        assert!(snapshot.reply_target().is_none(), "an edit ends the reply");
        assert_eq!(snapshot.compose, "Hi");

        snapshot.compose = "  ".into();
        assert!(!snapshot.can_send(), "an empty text");
        snapshot.send_compose();
        assert!(snapshot.take_commands().is_empty());
        assert!(snapshot.edit_target().is_some(), "the edit stays open");

        snapshot.compose = "Hello ".into();
        assert!(snapshot.can_send());
        snapshot.send_compose();
        assert_eq!(
            snapshot.take_commands(),
            vec![AdapterCommand::EditMessage {
                protocol: ProtocolId::Telegram,
                conversation_id: "telegram:1".into(),
                message_id: "telegram:1:6".into(),
                body: "Hello".into(),
            }]
        );
        assert!(snapshot.edit_target().is_none());
        assert_eq!(snapshot.compose, "draft");
        snapshot.send_compose();
        assert!(
            !snapshot
                .take_commands()
                .iter()
                .any(|command| matches!(command, AdapterCommand::EditMessage { .. })),
            "the next send is a new message"
        );
    }

    /// Cancel, a chat switch, the same text, and a delete each end the edit.
    /// The edit text never becomes the draft.
    #[test]
    fn an_edit_ends_with_the_draft_in_place() {
        let mut snapshot = telegram_chat_with_rows(&[(6, true)]);
        let edit = |snapshot: &mut Snapshot| {
            snapshot.begin_edit(ProtocolId::Telegram, "telegram:1", "telegram:1:6");
            assert!(snapshot.edit_target().is_some());
            snapshot.compose = "changed".into();
        };
        snapshot.compose = "draft".into();
        edit(&mut snapshot);
        snapshot.cancel_edit();
        assert_eq!(snapshot.compose, "draft");

        edit(&mut snapshot);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: test_support::telegram_chat(2, "Bob", 1),
        });
        snapshot.select_conversation("telegram:2".into());
        assert!(snapshot.compose.is_empty());
        snapshot.select_conversation("telegram:1".into());
        assert!(snapshot.edit_target().is_none(), "a chat switch ends it");
        assert_eq!(snapshot.compose, "draft", "the edit text is not a draft");

        edit(&mut snapshot);
        snapshot.compose = "Hi".into();
        snapshot.send_compose();
        assert_eq!(snapshot.compose, "draft");

        edit(&mut snapshot);
        snapshot.apply(AdapterEvent::MessagesRemoved {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            message_ids: vec!["telegram:1:6".into()],
        });
        assert!(snapshot.edit_target().is_none(), "the message is gone");
        assert_eq!(snapshot.compose, "draft");
        assert!(
            !snapshot
                .take_commands()
                .iter()
                .any(|command| matches!(command, AdapterCommand::EditMessage { .. })),
            "no edit left the app"
        );
    }

    /// `MessageEdited` and an incoming new text mark the row "edited".
    #[test]
    fn an_edit_event_marks_the_message() {
        let mut snapshot = telegram_chat_with_rows(&[(5, false), (6, false), (7, false)]);
        snapshot.apply(AdapterEvent::MessageEdited {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            message_id: "telegram:1:5".into(),
        });
        snapshot.apply(AdapterEvent::MessageBody {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            message_id: "telegram:1:6".into(),
            body: "new".into(),
        });
        let edited: Vec<bool> = snapshot
            .selected_messages()
            .iter()
            .map(|message| message.edited)
            .collect();
        assert_eq!(edited, vec![true, true, false]);
    }

    /// Edit offers only an own text message that the server has.
    #[test]
    fn edit_needs_an_own_sent_text() {
        let mut message = telegram_text(1, 5, "Hi");
        assert!(!can_edit(&message), "incoming");
        message.outbound = true;
        assert!(can_edit(&message));
        for delivery in [Delivery::Pending, Delivery::Failed] {
            message.delivery = delivery;
            assert!(!can_edit(&message), "{delivery:?}");
        }
        message.delivery = Delivery::Read;
        assert!(can_edit(&message));
        message.body = " ".into();
        assert!(!can_edit(&message), "no text");
    }

    fn typing_commands(snapshot: &mut Snapshot) -> Vec<(String, Option<MessageTopic>, bool)> {
        snapshot
            .take_commands()
            .into_iter()
            .filter_map(|command| match command {
                AdapterCommand::SetTyping {
                    protocol: ProtocolId::Telegram,
                    conversation_id,
                    topic,
                    typing,
                } => Some((conversation_id, topic, typing)),
                _ => None,
            })
            .collect()
    }

    /// The draft of the open chat sends "typing" at most one time each 5 s,
    /// and a draft that becomes empty sends a stop. A send sends no stop,
    /// and the next change sends "typing" again (#510).
    #[test]
    fn the_draft_of_the_open_chat_sends_the_typing_state() {
        let mut snapshot = telegram_chat_with_rows(&[(5, false)]);
        let (tg, chat) = (ProtocolId::Telegram, "telegram:1");
        let start = Instant::now();
        let at = |secs: u64| start + Duration::from_secs(secs);
        let write = |snapshot: &mut Snapshot, text: &str, at: Instant| {
            snapshot.set_draft(tg, chat, None, text.into());
            snapshot.send_typing(tg, chat, None, at);
        };
        let typing = |on: bool| vec![(chat.to_string(), None, on)];

        write(&mut snapshot, "H", at(0));
        write(&mut snapshot, "Hi", at(1));
        assert_eq!(typing_commands(&mut snapshot), typing(true));
        write(&mut snapshot, "Hi!", at(5));
        assert_eq!(typing_commands(&mut snapshot), typing(true));
        write(&mut snapshot, " ", at(6));
        assert_eq!(typing_commands(&mut snapshot), typing(false));
        write(&mut snapshot, "", at(6));
        assert_eq!(typing_commands(&mut snapshot), []);

        write(&mut snapshot, "Hello", at(7));
        assert_eq!(typing_commands(&mut snapshot), typing(true));
        snapshot.send_compose();
        assert_eq!(typing_commands(&mut snapshot), [], "a send sends no stop");
        write(&mut snapshot, "Hello again", at(8));
        assert_eq!(typing_commands(&mut snapshot), typing(true));

        snapshot.stop_typing(at(9));
        assert_eq!(typing_commands(&mut snapshot), typing(false));
        snapshot.stop_typing(at(9));
        assert_eq!(typing_commands(&mut snapshot), []);
    }

    /// No typing state for a draft of another chat, for an edit, or for a
    /// chat that this account cannot write in.
    #[test]
    fn typing_needs_the_open_writable_chat_and_no_edit() {
        let mut snapshot = telegram_chat_with_rows(&[(5, true)]);
        let tg = ProtocolId::Telegram;
        let start = Instant::now();
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: test_support::telegram_chat(2, "Bob", 0),
        });
        snapshot.set_draft(tg, "telegram:2", None, "Hi".into());
        snapshot.send_typing(tg, "telegram:2", None, start);
        assert_eq!(typing_commands(&mut snapshot), [], "another chat");

        snapshot.begin_edit(tg, "telegram:1", "telegram:1:5");
        assert!(snapshot.edit_target().is_some());
        snapshot.set_draft(tg, "telegram:1", None, "Hi there".into());
        snapshot.send_typing(tg, "telegram:1", None, start);
        assert_eq!(typing_commands(&mut snapshot), [], "an edit");
        snapshot.cancel_edit();

        let mut read_only = test_support::telegram_chat(1, "Ada", 1);
        read_only.writable = false;
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: read_only,
        });
        snapshot.set_draft(tg, "telegram:1", None, "Hi".into());
        snapshot.send_typing(tg, "telegram:1", None, start);
        assert_eq!(typing_commands(&mut snapshot), [], "read-only");
    }

    fn typing_event(chat: &str, topic: Option<MessageTopic>, who: &str, on: bool) -> AdapterEvent {
        AdapterEvent::Typing {
            protocol: ProtocolId::Telegram,
            conversation_id: chat.into(),
            topic,
            sender_id: format!("user:{who}"),
            sender: who.into(),
            typing: on,
        }
    }

    /// A person who types shows over the open thread until a stop, or 6 s
    /// with no event. A person of another chat or topic does not show. The
    /// end of the session forgets every typing state.
    #[test]
    fn a_person_who_types_shows_over_the_open_thread() {
        let mut snapshot = telegram_chat_with_rows(&[(5, false)]);
        let forum = Some(MessageTopic::Forum(3));
        snapshot.apply(typing_event("telegram:2", None, "Bob", true));
        snapshot.apply(typing_event("telegram:1", forum, "Cy", true));
        assert_eq!(snapshot.typing_line(), None);
        snapshot.apply(typing_event("telegram:1", None, "Ada", true));
        assert_eq!(snapshot.typing_line().as_deref(), Some("Ada is typing…"));
        snapshot.apply(typing_event("telegram:1", None, "Di", true));
        assert_eq!(
            snapshot.typing_line().as_deref(),
            Some("Ada and Di are typing…")
        );
        snapshot.apply(typing_event("telegram:1", None, "Ada", false));
        assert_eq!(snapshot.typing_line().as_deref(), Some("Di is typing…"));

        let mut message = telegram_text(1, 6, "Hello");
        message.sender = "Di".into();
        message.arrival = thinwire_protocol::Arrival::Live;
        snapshot.apply(typing_event("telegram:1", None, "Ada", true));
        snapshot.apply(AdapterEvent::MessageReceived { message });
        assert_eq!(
            snapshot.typing_line().as_deref(),
            Some("Ada is typing…"),
            "the message of Di ends the typing of Di"
        );

        let now = Instant::now();
        assert!(!snapshot.expire_typing(now));
        assert!(snapshot.expire_typing(now + Duration::from_secs(7)));
        assert_eq!(snapshot.typing_line(), None);

        snapshot.apply(typing_event("telegram:1", None, "Ada", true));
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Telegram,
            state: AccountState::Unlinked,
        });
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Telegram,
            state: AccountState::Linked,
        });
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: test_support::telegram_chat(1, "Ada", 1),
        });
        snapshot.select_conversation("telegram:1".into());
        assert_eq!(snapshot.typing_line(), None);
    }

    /// A Telegram chat "Ada" with messages 5, 6, and 7, and a second chat
    /// "Bob" to forward to.
    fn forward_chat() -> Snapshot {
        let mut snapshot = telegram_chat_with_rows(&[(5, false), (6, true), (7, false)]);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: test_support::telegram_chat(2, "Bob", 0),
        });
        snapshot.take_commands();
        snapshot
    }

    fn forward_command(snapshot: &mut Snapshot) -> Option<(Vec<String>, String)> {
        snapshot
            .take_commands()
            .into_iter()
            .find_map(|command| match command {
                AdapterCommand::ForwardMessages {
                    protocol: ProtocolId::Telegram,
                    from_conversation_id,
                    message_ids,
                    to_conversation_id,
                } if from_conversation_id == "telegram:1" => {
                    Some((message_ids, to_conversation_id))
                }
                _ => None,
            })
    }

    /// Select toggles messages of the open thread. Forward sends all of them
    /// to the chosen chat in one command, and the picker and the selection
    /// close.
    #[test]
    fn selected_messages_forward_in_one_command() {
        let mut snapshot = forward_chat();
        let (tg, chat) = (ProtocolId::Telegram, "telegram:1");
        assert!(!snapshot.selecting());
        snapshot.toggle_selection(tg, chat, "telegram:1:7");
        snapshot.toggle_selection(tg, chat, "telegram:1:5");
        snapshot.toggle_selection(tg, chat, "telegram:1:6");
        snapshot.toggle_selection(tg, chat, "telegram:1:6");
        snapshot.toggle_selection(tg, chat, "telegram:1:9");
        assert!(snapshot.selecting());
        assert_eq!(
            snapshot.selected_for_forward(),
            ["telegram:1:7", "telegram:1:5"],
            "a second click removes; an unknown id is not added"
        );

        snapshot.open_forward(tg, chat, None);
        let picker = snapshot.forward_picker().expect("picker");
        assert_eq!(picker.count, 2);
        let titles: Vec<&str> = picker
            .targets
            .iter()
            .map(|row| row.title.as_str())
            .collect();
        assert!(
            titles.contains(&"Ada") && titles.contains(&"Bob"),
            "{titles:?}"
        );
        snapshot.set_forward_query("bo".into());
        let picker = snapshot.forward_picker().expect("picker");
        let titles: Vec<&str> = picker
            .targets
            .iter()
            .map(|row| row.title.as_str())
            .collect();
        assert_eq!(titles, ["Bob"]);

        snapshot.forward_to("telegram:404");
        assert!(
            snapshot.forward_picker().is_some(),
            "not a chat of the account"
        );
        assert_eq!(forward_command(&mut snapshot), None);

        snapshot.forward_to("telegram:2");
        assert_eq!(
            forward_command(&mut snapshot),
            Some((
                vec!["telegram:1:7".to_string(), "telegram:1:5".to_string()],
                "telegram:2".to_string()
            ))
        );
        assert!(snapshot.forward_picker().is_none());
        assert!(!snapshot.selecting());
        assert_eq!(snapshot.status_text, "Forwarding 2 messages…");
    }

    /// Forward from the menu takes one message and keeps the selection.
    /// Cancel closes the picker with no command.
    #[test]
    fn one_message_forwards_from_the_menu() {
        let mut snapshot = forward_chat();
        let (tg, chat) = (ProtocolId::Telegram, "telegram:1");
        snapshot.toggle_selection(tg, chat, "telegram:1:5");
        snapshot.open_forward(tg, chat, Some("telegram:1:6"));
        assert_eq!(
            snapshot.forward_picker().map(|picker| picker.count),
            Some(1)
        );
        snapshot.cancel_forward();
        assert!(snapshot.forward_picker().is_none());
        assert_eq!(snapshot.selected_for_forward(), ["telegram:1:5"]);
        assert_eq!(forward_command(&mut snapshot), None);

        snapshot.open_forward(tg, chat, Some("telegram:1:6"));
        snapshot.forward_to("telegram:2");
        assert_eq!(
            forward_command(&mut snapshot),
            Some((vec!["telegram:1:6".to_string()], "telegram:2".to_string()))
        );
        assert_eq!(snapshot.status_text, "Forwarding the message…");
    }

    /// No forward for a send that waits or failed, for a protocol with no
    /// forward, or for a selection of another chat.
    #[test]
    fn forward_needs_a_sent_message_of_the_open_thread() {
        let mut snapshot = forward_chat();
        let (tg, chat) = (ProtocolId::Telegram, "telegram:1");
        if let Some(row) = snapshot
            .messages
            .values_mut()
            .flat_map(|rows| rows.iter_mut())
            .find(|row| row.id == "telegram:1:6")
        {
            row.delivery = Delivery::Pending;
        }
        snapshot.toggle_selection(tg, chat, "telegram:1:6");
        snapshot.open_forward(tg, chat, Some("telegram:1:6"));
        assert!(!snapshot.selecting(), "a send that waits");
        assert!(snapshot.forward_picker().is_none());

        snapshot.toggle_selection(tg, chat, "telegram:1:5");
        snapshot.select_conversation("telegram:2".into());
        assert!(!snapshot.selecting(), "the selection is of another chat");
        snapshot.open_forward(tg, "telegram:2", None);
        assert!(snapshot.forward_picker().is_none());
        snapshot.select_conversation("telegram:1".into());
        assert_eq!(snapshot.selected_for_forward(), ["telegram:1:5"]);
        snapshot.clear_selection();
        assert!(!snapshot.selecting());

        for row in &mut snapshot.accounts {
            row.caps.message_actions = thinwire_protocol::MessageActions::NONE;
        }
        snapshot.toggle_selection(tg, chat, "telegram:1:5");
        snapshot.open_forward(tg, chat, Some("telegram:1:5"));
        assert!(!snapshot.selecting());
        assert!(snapshot.forward_picker().is_none(), "no forward action");
    }

    /// Play on a clip with no file asks for its download once. The path
    /// comes back to the row, and a refresh of the row keeps it. A failed
    /// download marks the clip.
    #[test]
    fn play_video_downloads_a_clip_and_keeps_its_path() {
        use thinwire_protocol::{AudioPath, MessageVideo};

        let mut snapshot = Snapshot::new();
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Telegram,
            state: AccountState::Linked,
        });
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: test_support::telegram_chat(1, "Ada", 1),
        });
        snapshot.select_conversation("telegram:1".into());
        snapshot.take_commands();
        let clip = || {
            let mut message = telegram_text(1, 5, "Video");
            message.video = MessageVideo::new(12, 640, 360, 31, None);
            message
        };
        snapshot.apply(AdapterEvent::MessageReceived { message: clip() });
        let (chat, id) = ("telegram:1", "telegram:1:5");
        let video_of = |snapshot: &Snapshot| {
            snapshot
                .messages
                .values()
                .flatten()
                .find(|message| message.id == id)
                .and_then(|message| message.video.clone())
                .expect("clip")
        };
        snapshot.play_video(ProtocolId::Telegram, chat, id, 99);
        assert!(snapshot.take_commands().is_empty(), "another file");
        snapshot.play_video(ProtocolId::Telegram, chat, id, 31);
        assert_eq!(
            snapshot.take_commands(),
            vec![AdapterCommand::DownloadVideo {
                protocol: ProtocolId::Telegram,
                file_id: 31
            }]
        );
        snapshot.apply(AdapterEvent::VideoReady {
            protocol: ProtocolId::Telegram,
            file_id: 31,
            path: AudioPath::new("/tmp/clip.mp4".to_string()).expect("path"),
        });
        assert!(video_of(&snapshot).file().is_some());
        snapshot.play_video(ProtocolId::Telegram, chat, id, 31);
        assert!(snapshot.take_commands().is_empty(), "the file is here");
        // A refresh of the row with no path keeps the stored path.
        snapshot.apply(AdapterEvent::MessageReceived { message: clip() });
        assert!(video_of(&snapshot).file().is_some());

        let mut other = telegram_text(1, 6, "Video");
        other.video = MessageVideo::new(3, 0, 0, 32, None);
        snapshot.apply(AdapterEvent::MessageReceived { message: other });
        snapshot.apply(AdapterEvent::VideoFailed {
            protocol: ProtocolId::Telegram,
            file_id: 32,
        });
        let failed = snapshot
            .messages
            .values()
            .flatten()
            .find(|message| message.id == "telegram:1:6")
            .and_then(|message| message.video.clone())
            .expect("clip");
        assert!(failed.failed());
    }

    #[test]
    fn play_audio_downloads_a_voice_note_that_has_no_path() {
        use thinwire_protocol::{
            AudioPath, ImagePath, MessageAudio, MessagePhoto, PhotoFile, PhotoSize,
        };

        let mut snapshot = Snapshot::new();
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Telegram,
            state: AccountState::Linked,
        });
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: Conversation {
                protocol: ProtocolId::Telegram,
                id: "telegram:1".into(),
                title: "Ada".into(),
                participant: "Ada".into(),
                preview: String::new(),
                unread: 0,
                order: 1,
                last_at: 1,
                is_group: false,
                writable: true,
                placeholder: false,
                muted: false,
                has_topics: false,
                avatar: Default::default(),
            },
        });
        snapshot.select_conversation("telegram:1".into());
        snapshot.take_commands();
        let mut message = telegram_text(1, 5, "Voice message");
        let mut audio = MessageAudio::telegram(12, 42, None).expect("note");
        audio.mark_failed();
        message.audio = Some(audio);
        message.photo = Some(MessagePhoto {
            width: 100,
            height: 80,
            mini: None,
            sizes: vec![PhotoSize {
                width: 100,
                height: 80,
                file_id: Some(7),
                path: None,
                bytes: None,
            }],
            has_spoiler: false,
            is_secret: false,
            failed: false,
        });
        snapshot.apply(AdapterEvent::MessageReceived { message });
        let chat = "telegram:1";
        let id = "telegram:1:5";
        let audio_of = |snapshot: &Snapshot| {
            snapshot
                .messages
                .values()
                .flatten()
                .find(|message| message.id == id)
                .and_then(|message| message.audio.as_ref())
                .expect("voice")
                .clone()
        };
        snapshot.play_audio(ProtocolId::Slack, chat, id, 42);
        snapshot.play_audio(ProtocolId::Telegram, chat, id, 99);
        snapshot.play_audio(ProtocolId::Telegram, "telegram:9", id, 42);
        assert!(snapshot.take_commands().is_empty());
        snapshot.play_audio(ProtocolId::Telegram, chat, id, 42);
        snapshot.play_audio(ProtocolId::Telegram, chat, id, 42);
        let commands = snapshot.take_commands();
        assert_eq!(
            commands
                .iter()
                .filter(|command| matches!(
                    command,
                    AdapterCommand::DownloadVoice { file_id: 42, .. }
                ))
                .count(),
            2
        );
        assert!(
            !snapshot
                .photo_downloads
                .contains(&(ProtocolId::Telegram, 42))
        );
        assert!(!audio_of(&snapshot).failed());
        assert!(audio_of(&snapshot).file().is_none());
        snapshot.apply(AdapterEvent::PhotoReady {
            protocol: ProtocolId::Telegram,
            file_id: 7,
            file: PhotoFile::from_path(ImagePath::new("/tmp/hidden-photo")),
        });
        assert!(audio_of(&snapshot).file().is_none());
        let photo_path = snapshot
            .messages
            .values()
            .flatten()
            .find(|message| message.id == id)
            .and_then(|message| message.photo.as_ref())
            .and_then(|photo| photo.sizes.iter().find(|size| size.file_id == Some(7)))
            .and_then(|size| size.path.as_ref())
            .map(|path| path.as_str().to_string());
        assert_eq!(photo_path.as_deref(), Some("/tmp/hidden-photo"));
        let voice = AudioPath::new("/tmp/hidden-voice.ogg").expect("path");
        snapshot.apply(AdapterEvent::VoiceReady {
            protocol: ProtocolId::Telegram,
            file_id: 42,
            path: voice,
        });
        assert_eq!(
            audio_of(&snapshot).file().map(AudioPath::as_path),
            Some(std::path::Path::new("/tmp/hidden-voice.ogg"))
        );
        assert!(!audio_of(&snapshot).failed());
        let sizes = snapshot
            .messages
            .values()
            .flatten()
            .find(|message| message.id == id)
            .and_then(|message| message.photo.as_ref())
            .map(|photo| photo.sizes.clone())
            .expect("photo");
        assert!(sizes.iter().all(|size| {
            size.bytes.is_none()
                && size
                    .path
                    .as_ref()
                    .is_none_or(|path| path.as_str() != "/tmp/hidden-voice.ogg")
        }));
        snapshot.play_audio(ProtocolId::Telegram, chat, id, 42);
        assert!(snapshot.take_commands().is_empty());
        let mut kept = telegram_text(1, 5, "Voice message");
        kept.audio = MessageAudio::telegram(12, 42, None);
        snapshot.apply(AdapterEvent::MessageReceived { message: kept });
        assert_eq!(
            audio_of(&snapshot).file().map(AudioPath::as_path),
            Some(std::path::Path::new("/tmp/hidden-voice.ogg"))
        );
        let mut again = telegram_text(1, 5, "Voice message");
        again.audio = MessageAudio::telegram(12, 43, None);
        snapshot.apply(AdapterEvent::MessageReceived { message: again });
        snapshot.play_audio(ProtocolId::Telegram, chat, id, 43);
        assert!(
            snapshot.pending.iter().any(|command| matches!(
                command,
                AdapterCommand::DownloadVoice { file_id: 43, .. }
            ))
        );
        snapshot.end_session(ProtocolId::Telegram);
        assert!(
            !snapshot
                .pending
                .iter()
                .any(|command| matches!(command, AdapterCommand::DownloadVoice { .. }))
        );
    }

    #[test]
    fn a_slack_file_stop_stays_when_the_message_is_refreshed() {
        let mut snapshot = Snapshot::new();
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Telegram,
            state: AccountState::Linked,
        });
        let mut message = telegram_text(1, 5, "Voice message");
        message.audio = MessageAudio::telegram(0, 7, None);
        snapshot.apply(AdapterEvent::MessageReceived {
            message: message.clone(),
        });
        snapshot.apply(AdapterEvent::VoiceFailed {
            protocol: ProtocolId::Telegram,
            file_id: 7,
            stop: VoiceStop::SlackFilesDenied,
        });
        let stop_of = |snapshot: &Snapshot| {
            snapshot
                .messages
                .values()
                .flatten()
                .find(|row| row.id == "telegram:1:5")
                .and_then(|row| row.audio.as_ref())
                .and_then(|audio| audio.stop())
        };
        assert_eq!(stop_of(&snapshot), Some(VoiceStop::SlackFilesDenied));
        snapshot.apply(AdapterEvent::MessageReceived { message });
        assert_eq!(stop_of(&snapshot), Some(VoiceStop::SlackFilesDenied));
    }

    #[test]
    fn ending_the_session_lets_a_photo_download_start_again() {
        use thinwire_protocol::{MessagePhoto, PhotoSize};

        let mut snapshot = Snapshot::new();
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Telegram,
            state: AccountState::Linked,
        });
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: Conversation {
                protocol: ProtocolId::Telegram,
                id: "telegram:1".into(),
                title: "Ada".into(),
                participant: "Ada".into(),
                preview: String::new(),
                unread: 0,
                order: 1,
                last_at: 1,
                is_group: false,
                writable: true,
                placeholder: false,
                muted: false,
                has_topics: false,
                avatar: Default::default(),
            },
        });
        snapshot.select_conversation("telegram:1".into());
        snapshot.take_commands();
        let mut message = telegram_text(1, 2, "");
        message.photo = Some(MessagePhoto {
            width: 100,
            height: 80,
            mini: None,
            sizes: vec![PhotoSize {
                width: 100,
                height: 80,
                file_id: Some(7),
                path: None,
                bytes: None,
            }],
            has_spoiler: false,
            is_secret: false,
            failed: false,
        });
        snapshot.apply(AdapterEvent::MessageReceived { message });
        snapshot.load_photo(ProtocolId::Telegram, "telegram:1", "telegram:1:2", 7);
        snapshot.queue(AdapterCommand::DownloadFile {
            protocol: ProtocolId::Slack,
            file_id: 3,
            secret: false,
        });
        snapshot.photo_downloads.insert((ProtocolId::Slack, 3));
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Telegram,
            state: AccountState::Unlinked,
        });
        assert!(
            !snapshot
                .photo_downloads
                .contains(&(ProtocolId::Telegram, 7)),
            "the cancelled Telegram download is still pending"
        );
        assert!(
            snapshot.photo_downloads.contains(&(ProtocolId::Slack, 3)),
            "ending Telegram cleared a Slack download"
        );
        assert!(snapshot.pending.iter().any(|command| matches!(
            command,
            AdapterCommand::DownloadFile {
                protocol: ProtocolId::Slack,
                file_id: 3,
                ..
            }
        )));
        assert!(
            !snapshot.pending.iter().any(|command| matches!(
                command,
                AdapterCommand::DownloadFile {
                    protocol: ProtocolId::Telegram,
                    ..
                }
            )),
            "a queued Telegram download survived the session end"
        );
        snapshot.take_commands();

        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Telegram,
            state: AccountState::Linked,
        });
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: Conversation {
                protocol: ProtocolId::Telegram,
                id: "telegram:1".into(),
                title: "Ada".into(),
                participant: "Ada".into(),
                preview: String::new(),
                unread: 0,
                order: 1,
                last_at: 1,
                is_group: false,
                writable: true,
                placeholder: false,
                muted: false,
                has_topics: false,
                avatar: Default::default(),
            },
        });
        snapshot.select_conversation("telegram:1".into());
        snapshot.take_commands();
        let mut again = telegram_text(1, 2, "");
        again.photo = Some(MessagePhoto {
            width: 100,
            height: 80,
            mini: None,
            sizes: vec![PhotoSize {
                width: 100,
                height: 80,
                file_id: Some(7),
                path: None,
                bytes: None,
            }],
            has_spoiler: false,
            is_secret: false,
            failed: false,
        });
        snapshot.apply(AdapterEvent::MessageReceived { message: again });
        snapshot.load_photo(ProtocolId::Telegram, "telegram:1", "telegram:1:2", 7);
        let downloads = |commands: &[AdapterCommand]| {
            commands
                .iter()
                .filter(|command| {
                    matches!(command, AdapterCommand::DownloadFile { file_id: 7, .. })
                })
                .count()
        };
        assert_eq!(downloads(&snapshot.take_commands()), 1);
    }

    #[test]
    fn a_secret_photo_stores_bytes_and_drops_the_path() {
        use thinwire_protocol::{ImageBytes, ImagePath, MessagePhoto, PhotoSize};

        let mut snapshot = Snapshot::new();
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Telegram,
            state: AccountState::Linked,
        });
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: Conversation {
                protocol: ProtocolId::Telegram,
                id: "telegram:1".into(),
                title: "Ada".into(),
                participant: "Ada".into(),
                preview: String::new(),
                unread: 0,
                order: 1,
                last_at: 1,
                is_group: false,
                writable: true,
                placeholder: false,
                muted: false,
                has_topics: false,
                avatar: Default::default(),
            },
        });
        snapshot.select_conversation("telegram:1".into());
        snapshot.take_commands();
        let mut message = telegram_text(1, 2, "");
        message.photo = Some(MessagePhoto {
            width: 80,
            height: 80,
            mini: None,
            sizes: vec![PhotoSize {
                width: 80,
                height: 80,
                file_id: Some(9),
                path: None,
                bytes: None,
            }],
            has_spoiler: false,
            is_secret: true,
            failed: false,
        });
        snapshot.apply(AdapterEvent::MessageReceived { message });
        snapshot.load_photo(ProtocolId::Telegram, "telegram:1", "telegram:1:2", 9);
        let queued = snapshot.take_commands();
        assert!(queued.iter().any(|command| matches!(
            command,
            AdapterCommand::DownloadFile {
                file_id: 9,
                secret: true,
                ..
            }
        )));
        let file = thinwire_protocol::PhotoFile::new(
            Some(ImagePath::new("/tmp/secret-photo")),
            Some(ImageBytes::new(b"jpeg-bytes".to_vec())),
        )
        .expect("path and bytes");
        snapshot.apply(AdapterEvent::PhotoReady {
            protocol: ProtocolId::Telegram,
            file_id: 9,
            file,
        });
        let photo = snapshot
            .messages
            .values()
            .flatten()
            .find(|message| message.id == "telegram:1:2")
            .and_then(|message| message.photo.clone())
            .expect("photo");
        let size = &photo.sizes[0];
        assert!(size.path.is_none());
        assert_eq!(
            size.bytes.as_ref().map(ImageBytes::as_slice),
            Some(b"jpeg-bytes".as_slice())
        );
        assert!(!format!("{photo:?}").contains("secret-photo"));
        assert!(!format!("{photo:?}").contains("jpeg-bytes"));
    }

    #[test]
    fn a_new_avatar_file_id_drops_the_downloaded_path() {
        use thinwire_protocol::ImagePath;

        let mut snapshot = Snapshot::new();
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Telegram,
            state: AccountState::Linked,
        });
        let mut row = telegram_chat(1, "Ada", 1);
        row.avatar.file_id = Some(4);
        snapshot.apply(AdapterEvent::ConversationUpsert { conversation: row });
        snapshot.load_avatar(ProtocolId::Telegram, "telegram:1", 4);
        snapshot.load_avatar(ProtocolId::Telegram, "telegram:1", 4);
        assert!(
            snapshot
                .conversation(ProtocolId::Telegram, "telegram:1")
                .expect("row")
                .avatar
                .requested,
            "the row remembers the ask"
        );
        let queued = snapshot.take_commands();
        assert_eq!(
            queued
                .iter()
                .filter(|command| matches!(
                    command,
                    AdapterCommand::DownloadFile { file_id: 4, .. }
                ))
                .count(),
            1,
            "one download while the file is in flight"
        );
        let file = thinwire_protocol::PhotoFile::new(Some(ImagePath::new("/tmp/avatar-a")), None)
            .expect("path");
        snapshot.apply(AdapterEvent::PhotoReady {
            protocol: ProtocolId::Telegram,
            file_id: 4,
            file,
        });
        let kept = snapshot
            .conversation(ProtocolId::Telegram, "telegram:1")
            .expect("row");
        assert!(kept.avatar.path.is_some());
        assert!(!format!("{kept:?}").contains("avatar-a"));

        let mut same = telegram_chat(1, "Ada Park", 1);
        same.avatar.file_id = Some(4);
        snapshot.apply(AdapterEvent::ConversationUpsert { conversation: same });
        let kept_ask = snapshot
            .conversation(ProtocolId::Telegram, "telegram:1")
            .expect("row");
        assert!(
            kept_ask.avatar.path.is_some(),
            "the same file id keeps the download"
        );
        assert!(kept_ask.avatar.requested, "the same file id keeps the ask");

        let mut next = telegram_chat(1, "Ada Park", 1);
        next.avatar.file_id = Some(5);
        snapshot.apply(AdapterEvent::ConversationUpsert { conversation: next });
        let row = snapshot
            .conversation(ProtocolId::Telegram, "telegram:1")
            .expect("row");
        assert_eq!(row.avatar.file_id, Some(5));
        assert!(row.avatar.path.is_none(), "the old file id is gone");
        assert!(!row.avatar.requested, "a new file id asks again");
    }

    #[test]
    fn an_empty_photo_file_is_not_a_finished_avatar() {
        use thinwire_protocol::{ImageBytes, PhotoFile};

        assert!(PhotoFile::new(None, None).is_none());
        assert!(PhotoFile::new(None, Some(ImageBytes::new(Vec::new()))).is_none());

        let mut snapshot = Snapshot::new();
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Telegram,
            state: AccountState::Linked,
        });
        let mut row = telegram_chat(1, "Ada", 1);
        row.avatar.file_id = Some(4);
        snapshot.apply(AdapterEvent::ConversationUpsert { conversation: row });
        snapshot.load_avatar(ProtocolId::Telegram, "telegram:1", 4);
        snapshot.take_commands();
        let file = PhotoFile::from_bytes(ImageBytes::new(b"jpeg".to_vec())).expect("bytes");
        snapshot.apply(AdapterEvent::PhotoReady {
            protocol: ProtocolId::Telegram,
            file_id: 4,
            file,
        });
        let avatar = snapshot
            .conversation(ProtocolId::Telegram, "telegram:1")
            .expect("row")
            .avatar
            .clone();
        assert!(avatar.ready());
        assert!(!avatar.failed);
        assert!(avatar.path.is_none());
        snapshot.load_avatar(ProtocolId::Telegram, "telegram:1", 4);
        assert!(
            snapshot.take_commands().is_empty(),
            "a ready avatar does not download again"
        );

        snapshot.load_avatar(ProtocolId::Discord, "discord:1", 4);
        assert!(
            snapshot.take_commands().is_empty(),
            "another protocol does not download an avatar"
        );
    }

    #[test]
    fn a_failed_avatar_stays_failed_for_the_session() {
        let mut snapshot = Snapshot::new();
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Telegram,
            state: AccountState::Linked,
        });
        let mut row = telegram_chat(1, "Ada", 1);
        row.avatar.file_id = Some(4);
        snapshot.apply(AdapterEvent::ConversationUpsert { conversation: row });
        snapshot.apply(AdapterEvent::PhotoFailed {
            protocol: ProtocolId::Telegram,
            file_id: 4,
        });
        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Telegram,
            status: AdapterStatus::Ready,
            detail: "Message sent.".into(),
        });
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Telegram,
            state: AccountState::Linked,
        });
        let avatar = snapshot
            .conversation(ProtocolId::Telegram, "telegram:1")
            .expect("row")
            .avatar
            .clone();
        assert!(avatar.failed);
        snapshot.take_commands();
        snapshot.load_avatar(ProtocolId::Telegram, "telegram:1", 4);
        let downloads = snapshot.take_commands();
        assert!(
            downloads.iter().all(|command| {
                !matches!(command, AdapterCommand::DownloadFile { file_id: 4, .. })
            }),
            "a failed avatar does not download again: {downloads:?}"
        );
    }

    fn forum_row(
        chat: &str,
        topic: MessageTopic,
        name: &str,
        unread: u32,
        last_at: i64,
    ) -> ChatTopic {
        ChatTopic {
            protocol: ProtocolId::Telegram,
            conversation_id: chat.into(),
            topic,
            name: name.into(),
            unread,
            last_at,
            general: false,
            hidden: false,
            closed: false,
            muted: false,
        }
    }

    #[test]
    fn inbox_state_covers_loading_rows_empty_and_no_match() {
        let store = SecretStore::memory();
        let mut snapshot = Snapshot::new();
        assert_eq!(snapshot.inbox_state(), InboxState::Empty);
        complete_telegram(&mut snapshot, &store);
        assert_eq!(snapshot.inbox_state(), InboxState::Loading);
        snapshot.apply(AdapterEvent::ChatListLoaded {
            protocol: ProtocolId::Telegram,
        });
        assert_eq!(snapshot.inbox_state(), InboxState::Empty);

        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: telegram_chat(1, "Ada", 10),
        });
        assert_eq!(snapshot.inbox_state(), InboxState::Rows);
        snapshot.search = "zzz".into();
        assert_eq!(snapshot.inbox_state(), InboxState::NoMatch);
        snapshot.search = "ad".into();
        assert_eq!(snapshot.inbox_state(), InboxState::Rows);

        snapshot.refresh_visible();
        snapshot.apply(AdapterEvent::ConversationRemoved {
            protocol: ProtocolId::Telegram,
            id: "telegram:1".into(),
        });
        assert_eq!(snapshot.inbox_state(), InboxState::Loading);
        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Telegram,
            status: AdapterStatus::Error,
            detail: "Could not load Telegram chats (TDLib 500).".into(),
        });
        assert_eq!(snapshot.inbox_state(), InboxState::Empty);
    }

    /// The middle column lists the selected account. Another account's chats
    /// stay off that list. Both accounts stay in the sidebar.
    #[test]
    fn the_middle_column_lists_only_the_selected_account() {
        let mut snapshot = Snapshot::new();
        test_support::show_protocol(&mut snapshot, ProtocolId::Slack);
        link(&mut snapshot, ProtocolId::Telegram);
        link(&mut snapshot, ProtocolId::Slack);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Telegram, "ada", true),
        });
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Slack, "general", true),
        });
        snapshot.select_protocol(ProtocolId::Telegram);
        assert_eq!(visible_ids(&snapshot), ["ada"]);
        snapshot.select_protocol(ProtocolId::Slack);
        assert_eq!(visible_ids(&snapshot), ["general"]);
        assert!(snapshot.shows_in_switcher(ProtocolId::Telegram));
        assert!(snapshot.shows_in_switcher(ProtocolId::Slack));
    }

    /// Loading while the account syncs. "No chats yet." only after the link,
    /// when the list is empty. Before the link the column has no empty line.
    #[test]
    fn the_middle_column_empty_states_follow_the_link() {
        let store = SecretStore::memory();
        let mut snapshot = Snapshot::new();
        assert_eq!(snapshot.inbox_placeholder(), None);
        complete_telegram(&mut snapshot, &store);
        assert_eq!(snapshot.inbox_placeholder(), Some(LOADING_CHATS_STATUS));
        snapshot.apply(AdapterEvent::ChatListLoaded {
            protocol: ProtocolId::Telegram,
        });
        assert_eq!(snapshot.inbox_placeholder(), Some(NO_CHATS_YET));
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: telegram_chat(1, "Ada", 1),
        });
        assert_eq!(snapshot.inbox_placeholder(), None);
    }

    /// Unofficial and Ban risk open the README text. They do not start a link.
    #[test]
    fn unofficial_opens_the_readme_notice_without_starting_a_link() {
        let mut snapshot = Snapshot::new();
        snapshot.open_notice_review(ProtocolId::Telegram);
        assert!(snapshot.notice_review().is_none());
        snapshot.open_notice_review(ProtocolId::WhatsApp);
        assert_eq!(snapshot.notice_review(), Some(ProtocolId::WhatsApp));
        let readme = include_str!("../../../README.md");
        for protocol in [ProtocolId::WhatsApp, ProtocolId::Signal] {
            let lines = unofficial_notice(protocol).expect("notice");
            assert!(!lines.is_empty());
            for line in lines {
                assert!(readme.contains(line), "{line}");
                assert!(
                    !line.contains("full-screen notice with this text"),
                    "{line}"
                );
            }
        }
        assert_eq!(notice_label(ProtocolId::WhatsApp), Some("Ban risk"));
        assert_eq!(notice_label(ProtocolId::Signal), Some("Risk"));
        assert_eq!(notice_label(ProtocolId::Telegram), None);
        assert!(snapshot.take_commands().is_empty());
        snapshot.close_notice_review();
        assert!(snapshot.notice_review().is_none());
    }

    #[cfg(feature = "whatsapp-web")]
    #[test]
    fn the_whatsapp_notice_gates_linking_and_a_review_does_not_cancel_it() {
        let phone = thinwire_protocol::WhatsAppPhoneVault::new();
        let mut snapshot = Snapshot::new();
        snapshot.begin_whatsapp_link(&phone);
        assert!(
            !snapshot
                .take_commands()
                .iter()
                .any(|command| matches!(command, AdapterCommand::WhatsAppBeginLink { .. })),
            "linking waits for the notice"
        );
        snapshot.open_whatsapp_risk_gate();
        snapshot.begin_whatsapp_link(&phone);
        assert!(!snapshot.whatsapp_started);
        assert!(snapshot.whatsapp_qr.is_none());
        snapshot.acknowledge_whatsapp_risk();
        assert_eq!(snapshot.whatsapp_screen, WhatsAppScreen::Pair);
        assert!(snapshot.whatsapp_qr.is_none(), "confirm is before any QR");
        snapshot.begin_whatsapp_link(&phone);
        assert!(snapshot.whatsapp_started);
        let _ = snapshot.take_commands();
        snapshot.open_notice_review(ProtocolId::WhatsApp);
        assert_eq!(snapshot.notice_review(), Some(ProtocolId::WhatsApp));
        assert!(snapshot.whatsapp_started);
        assert!(
            !snapshot
                .take_commands()
                .contains(&AdapterCommand::WhatsAppCancelLink)
        );
    }

    #[cfg(feature = "signal-local")]
    #[test]
    fn the_signal_notice_gates_linking() {
        let mut snapshot = Snapshot::new();
        snapshot.begin_signal_link();
        assert!(
            !snapshot
                .take_commands()
                .iter()
                .any(|command| matches!(command, AdapterCommand::SignalBeginLink { .. }))
        );
        snapshot.open_signal_notice();
        snapshot.begin_signal_link();
        assert!(!snapshot.signal_started);
        assert!(snapshot.signal_qr.is_none());
        snapshot.acknowledge_signal_notice();
        assert_eq!(snapshot.signal_screen, SignalScreen::Link);
        assert!(snapshot.signal_qr.is_none());
        snapshot.begin_signal_link();
        assert!(snapshot.signal_started);
        let _ = snapshot.take_commands();
        snapshot.open_notice_review(ProtocolId::Signal);
        assert_eq!(snapshot.notice_review(), Some(ProtocolId::Signal));
        assert!(snapshot.signal_started);
        assert!(
            !snapshot
                .take_commands()
                .contains(&AdapterCommand::SignalCancelLink)
        );
    }

    fn visible_ids(snapshot: &Snapshot) -> Vec<&str> {
        snapshot
            .visible_conversations()
            .iter()
            .map(|row| row.id.as_str())
            .collect()
    }

    #[test]
    fn thread_state_covers_loading_rows_and_empty() {
        let store = SecretStore::memory();
        let mut snapshot = Snapshot::new();
        complete_telegram(&mut snapshot, &store);
        assert_eq!(snapshot.thread_state(), ThreadState::NoSelection);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: telegram_chat(1, "Ada", 10),
        });
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: telegram_chat(2, "Bob", 5),
        });
        assert_eq!(
            snapshot.selected_conversation.as_deref(),
            Some("telegram:1")
        );
        assert_eq!(snapshot.thread_state(), ThreadState::Loading);
        snapshot.apply(AdapterEvent::HistoryLoaded {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            store_cursor: None,
        });
        assert_eq!(snapshot.thread_state(), ThreadState::Empty);

        snapshot.select_conversation("telegram:2".into());
        assert_eq!(snapshot.thread_state(), ThreadState::Loading);
        snapshot.apply(AdapterEvent::MessageReceived {
            message: telegram_text(2, 7, "hi"),
        });
        assert_eq!(snapshot.thread_state(), ThreadState::Rows);
    }

    #[test]
    fn picking_a_chat_scrolls_it_into_view_and_a_reorder_does_not() {
        let store = SecretStore::memory();
        let mut snapshot = Snapshot::new();
        complete_telegram(&mut snapshot, &store);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: telegram_chat(1, "Ada", 10),
        });
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: telegram_chat(2, "Bob", 5),
        });
        assert!(
            !snapshot.wants_scroll_to_selected(),
            "the first row is not a pick"
        );
        snapshot.select_conversation("telegram:2".into());
        assert!(snapshot.take_scroll_to_selected());
        assert!(!snapshot.wants_scroll_to_selected(), "one request per pick");

        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: telegram_chat(1, "Ada", 11),
        });
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: telegram_chat(3, "Cy", 99),
        });
        assert_eq!(
            snapshot.selected_conversation.as_deref(),
            Some("telegram:2"),
            "a row that jumps to the top does not become the open chat"
        );
        assert!(
            !snapshot.wants_scroll_to_selected(),
            "a reorder does not scroll to the open chat"
        );
        assert!(
            !snapshot.wants_scroll_to_focused(),
            "a reorder does not scroll to the highlight"
        );
    }

    /// Chat B is open. A message in chat A moves A to the top. B stays open,
    /// its draft stays, and A shows the unread badge. A message in B does not
    /// mark B unread.
    #[test]
    fn a_message_in_another_chat_keeps_the_open_chat() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        snapshot.select_conversation("telegram:2".into());
        assert!(snapshot.take_scroll_to_selected());
        assert!(snapshot.take_focus_compose());
        snapshot.compose = "draft for Bob".into();
        snapshot.sync_viewed();
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: telegram_chat(2, "Bob", 20),
        });
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: telegram_chat(1, "Ada", 5),
        });
        assert_eq!(
            snapshot.visible_conversations()[0].id,
            "telegram:2",
            "Bob starts above Ada"
        );
        assert!(!snapshot.wants_scroll_to_selected());
        assert!(!snapshot.wants_scroll_to_focused());

        let mut ada = telegram_chat(1, "Ada", 40);
        ada.unread = 2;
        ada.preview = "new from Ada".into();
        ada.last_at = 1_700_000_100;
        snapshot.apply(AdapterEvent::ConversationUpsert { conversation: ada });

        assert_eq!(
            snapshot.selected_conversation.as_deref(),
            Some("telegram:2")
        );
        assert_eq!(snapshot.compose, "draft for Bob");
        assert!(!snapshot.wants_focus_compose());
        assert!(!snapshot.wants_scroll_to_selected());
        assert!(!snapshot.wants_scroll_to_focused());
        let rows = snapshot.visible_conversations();
        assert_eq!(rows[0].id, "telegram:1", "Ada moved to the top");
        assert_eq!(rows[0].unread, 2);
        assert_eq!(rows[0].preview, "new from Ada");
        assert_eq!(rows[0].last_at, 1_700_000_100);
        assert_eq!(rows[1].id, "telegram:2");

        let mut bob = telegram_chat(2, "Bob", 5);
        bob.unread = 4;
        bob.preview = "while reading".into();
        bob.last_at = 1_700_000_200;
        snapshot.apply(AdapterEvent::ConversationUpsert { conversation: bob });
        let bob = snapshot
            .conversation(ProtocolId::Telegram, "telegram:2")
            .expect("Bob");
        assert_eq!(bob.unread, 0, "the open chat is not marked unread");
        assert_eq!(bob.preview, "while reading");
        assert_eq!(
            snapshot.selected_conversation.as_deref(),
            Some("telegram:2")
        );
        assert_eq!(snapshot.compose, "draft for Bob");
        assert!(!snapshot.wants_scroll_to_selected());
    }

    #[test]
    fn a_message_in_another_topic_keeps_the_open_topic_and_its_draft() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        let mut camp = telegram_chat(9, "Camp forum", 30);
        camp.has_topics = true;
        camp.is_group = true;
        snapshot.apply(AdapterEvent::ConversationUpsert { conversation: camp });
        let trail = MessageTopic::Forum(2);
        let food = MessageTopic::Forum(3);
        snapshot.apply(AdapterEvent::TopicUpsert {
            topic: forum_row("telegram:9", trail, "Trail", 1, 100),
        });
        snapshot.apply(AdapterEvent::TopicUpsert {
            topic: forum_row("telegram:9", food, "Food", 0, 50),
        });
        snapshot.select_conversation("telegram:9".into());
        snapshot.select_topic("telegram:9".into(), trail);
        assert!(snapshot.take_focus_compose());
        assert!(snapshot.take_scroll_to_selected());
        snapshot.compose = "draft for Trail".into();
        snapshot.sync_viewed();

        let mut message = telegram_text(9, 8, "soup");
        message.topic = Some(food);
        message.arrival = thinwire_protocol::Arrival::Live;
        message.sent_at = 200;
        snapshot.apply(AdapterEvent::MessageReceived { message });

        assert_eq!(snapshot.selected_topic, Some(trail));
        assert_eq!(snapshot.compose, "draft for Trail");
        assert!(snapshot.selected_messages().is_empty());
        assert!(!snapshot.wants_focus_compose());
        assert!(!snapshot.wants_scroll_to_selected());
        assert!(!snapshot.wants_scroll_to_focused());
        let food_row = snapshot
            .topic(ProtocolId::Telegram, "telegram:9", food)
            .expect("food");
        assert_eq!(food_row.unread, 1);
        assert_eq!(food_row.last_at, 200);

        snapshot.center_key(AuthKey::Escape, &store);
        assert!(snapshot.selected_topic.is_none());
        snapshot.select_topic("telegram:9".into(), trail);
        assert_eq!(snapshot.compose, "draft for Trail");
    }

    #[test]
    fn a_forum_badge_skips_muted_topics_and_keeps_general_first() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        let mut camp = telegram_chat(9, "Camp forum", 8);
        camp.has_topics = true;
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: camp.clone(),
        });
        snapshot.apply(AdapterEvent::TopicUpsert {
            topic: forum_row("telegram:9", MessageTopic::Forum(2), "Trail", 2, 300),
        });
        let mut general = forum_row("telegram:9", MessageTopic::Forum(1), "General", 1, 100);
        general.general = true;
        snapshot.apply(AdapterEvent::TopicUpsert { topic: general });
        let mut hidden = forum_row("telegram:9", MessageTopic::Forum(8), "Hidden", 5, 400);
        hidden.hidden = true;
        hidden.general = true;
        snapshot.apply(AdapterEvent::TopicUpsert { topic: hidden });
        let mut protocol_muted = forum_row("telegram:9", MessageTopic::Forum(4), "Archive", 7, 200);
        protocol_muted.muted = true;
        snapshot.apply(AdapterEvent::TopicUpsert {
            topic: protocol_muted,
        });
        snapshot.apply(AdapterEvent::TopicUpsert {
            topic: forum_row("telegram:9", MessageTopic::Forum(3), "Food", 4, 250),
        });
        let mut mutes = ChatMutes::in_memory();
        mutes.set(
            ProtocolId::Telegram,
            &topic_mute_id("telegram:9", MessageTopic::Forum(3)),
            true,
        );
        let row = snapshot
            .conversation(ProtocolId::Telegram, "telegram:9")
            .expect("camp")
            .clone();
        // General 1 + Trail 2 + hidden 5. Food is muted here. Archive is muted
        // in the protocol. The chat's own unread is not the badge.
        assert_eq!(snapshot.display_unread(&row, &mutes), 8);
        snapshot.select_conversation("telegram:9".into());
        let names: Vec<&str> = snapshot
            .selected_topics()
            .iter()
            .map(|topic| topic.name.as_str())
            .collect();
        assert_eq!(names, ["General", "Trail", "Food", "Archive"]);
        let _ = store;
    }

    /// A forum with no topic rows yet still shows its own unread, on the row
    /// and on the account chip. Loaded topics replace that count.
    #[test]
    fn a_forum_badge_uses_the_chat_unread_until_topics_load() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        let mut camp = telegram_chat(9, "Camp forum", 30);
        camp.has_topics = true;
        camp.unread = 6;
        snapshot.apply(AdapterEvent::ConversationUpsert { conversation: camp });
        let mutes = ChatMutes::in_memory();
        let row = snapshot
            .conversation(ProtocolId::Telegram, "telegram:9")
            .expect("camp")
            .clone();
        assert_eq!(snapshot.display_unread(&row, &mutes), 6);
        assert_eq!(
            snapshot.account_display_unread(ProtocolId::Telegram, &mutes),
            6
        );
        snapshot.apply(AdapterEvent::TopicUpsert {
            topic: forum_row("telegram:9", MessageTopic::Forum(2), "Trail", 2, 10),
        });
        let mut quiet = forum_row("telegram:9", MessageTopic::Forum(3), "Quiet", 9, 20);
        quiet.muted = true;
        snapshot.apply(AdapterEvent::TopicUpsert { topic: quiet });
        let row = snapshot
            .conversation(ProtocolId::Telegram, "telegram:9")
            .expect("camp")
            .clone();
        assert_eq!(snapshot.display_unread(&row, &mutes), 2);
        assert_eq!(
            snapshot.account_display_unread(ProtocolId::Telegram, &mutes),
            2
        );
    }

    /// Photos and videos go out in one media send with the text as the
    /// caption. The tray keeps them until the adapter accepts the send.
    #[test]
    fn attached_files_go_out_with_the_text_and_leave_the_tray_on_accept() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        snapshot.select_conversation("telegram:1".into());
        snapshot.take_commands();
        snapshot.add_attachments(
            ProtocolId::Telegram,
            "telegram:1",
            None,
            vec![
                "/home/a/cat.png".into(),
                "/home/a/notes.pdf".into(),
                "/home/a/clip.mp4".into(),
                "/home/a/cat.png".into(),
            ],
        );
        assert_eq!(snapshot.compose_attachments().len(), 2, "no pdf, no repeat");
        assert_eq!(snapshot.attachment_note(), Some(ATTACH_ONLY_MEDIA));
        assert!(snapshot.can_send(), "files with no text can go");
        snapshot.send_compose();
        let commands = snapshot.take_commands();
        let Some(AdapterCommand::SendMedia {
            files,
            caption,
            request,
            topic,
            ..
        }) = commands.last()
        else {
            panic!("no media send: {commands:?}");
        };
        assert_eq!(files.len(), 2);
        assert_eq!(caption, "");
        assert_eq!(*topic, None);
        assert_eq!(
            snapshot.compose_attachments().len(),
            2,
            "the tray stays until the adapter accepts"
        );
        assert!(!snapshot.can_send(), "one send at a time");
        snapshot.apply(AdapterEvent::SendAccepted {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            request: *request,
        });
        assert!(snapshot.compose_attachments().is_empty());

        // A rejection keeps the tray. Remove takes one file out.
        snapshot.add_attachments(
            ProtocolId::Telegram,
            "telegram:1",
            None,
            vec!["/a/1.jpg".into(), "/a/2.jpg".into()],
        );
        snapshot.compose = "look".into();
        snapshot.send_compose();
        let request = snapshot
            .take_commands()
            .into_iter()
            .find_map(|command| match command {
                AdapterCommand::SendMedia {
                    request, caption, ..
                } if caption == "look" => Some(request),
                _ => None,
            })
            .expect("media send");
        snapshot.apply(AdapterEvent::SendRejected {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            request,
        });
        assert_eq!(snapshot.compose_attachments().len(), 2);
        snapshot.remove_attachment(ProtocolId::Telegram, "telegram:1", None, 0);
        assert_eq!(snapshot.compose_attachments().len(), 1);
        assert_eq!(
            snapshot.compose_attachments()[0].path,
            std::path::PathBuf::from("/a/2.jpg")
        );
    }

    /// The tray of one chat does not show in another chat, and it holds 30
    /// files at most.
    #[test]
    fn the_tray_belongs_to_its_chat_and_is_capped() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        snapshot.select_conversation("telegram:1".into());
        let many = (0..40).map(|n| format!("/a/{n}.png").into()).collect();
        snapshot.add_attachments(ProtocolId::Telegram, "telegram:1", None, many);
        assert_eq!(snapshot.compose_attachments().len(), MAX_ATTACHMENTS);
        assert_eq!(snapshot.attachment_note(), Some(ATTACH_TOO_MANY));
        snapshot.select_conversation("telegram:2".into());
        assert!(snapshot.compose_attachments().is_empty());
        snapshot.select_conversation("telegram:1".into());
        assert_eq!(snapshot.compose_attachments().len(), MAX_ATTACHMENTS);
    }

    /// An accept for topic A clears A's draft. Topic B keeps the same text.
    #[test]
    fn an_accepted_send_clears_only_that_topics_draft() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        let mut camp = telegram_chat(9, "Camp forum", 30);
        camp.has_topics = true;
        snapshot.apply(AdapterEvent::ConversationUpsert { conversation: camp });
        let trail = MessageTopic::Forum(2);
        let food = MessageTopic::Forum(3);
        let gear = MessageTopic::Forum(4);
        for (topic, name) in [(trail, "Trail"), (food, "Food"), (gear, "Gear")] {
            snapshot.apply(AdapterEvent::TopicUpsert {
                topic: forum_row("telegram:9", topic, name, 0, 1),
            });
        }
        snapshot.select_conversation("telegram:9".into());
        snapshot.select_topic("telegram:9".into(), trail);
        snapshot.compose = "ok".into();
        snapshot.send_compose();
        let request = snapshot
            .take_commands()
            .into_iter()
            .find_map(|command| match command {
                AdapterCommand::SendTopicText {
                    request,
                    topic,
                    body,
                    ..
                } if topic == trail && body == "ok" => Some(request),
                _ => None,
            });
        let request = request.expect("send");
        snapshot.select_topic("telegram:9".into(), food);
        snapshot.compose = "ok".into();
        snapshot.set_draft(ProtocolId::Telegram, "telegram:9", Some(gear), "ok".into());
        snapshot.apply(AdapterEvent::SendAccepted {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:9".into(),
            request,
        });
        assert_eq!(snapshot.compose, "ok", "Food keeps its unsent text");
        assert!(
            !snapshot.drafts.contains_key(&(
                ProtocolId::Telegram,
                "telegram:9".into(),
                Some(trail)
            )),
            "Trail's sent text is gone"
        );
        assert_eq!(
            snapshot
                .drafts
                .get(&(ProtocolId::Telegram, "telegram:9".into(), Some(gear)))
                .map(String::as_str),
            Some("ok")
        );
        let _ = store;
    }

    /// `more == false` for topic A does not stop topic B from paging.
    #[test]
    fn an_older_page_for_one_topic_does_not_end_another() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        let mut camp = telegram_chat(9, "Camp forum", 30);
        camp.has_topics = true;
        snapshot.apply(AdapterEvent::ConversationUpsert { conversation: camp });
        let trail = MessageTopic::Forum(2);
        let food = MessageTopic::Forum(3);
        snapshot.apply(AdapterEvent::TopicUpsert {
            topic: forum_row("telegram:9", trail, "Trail", 0, 2),
        });
        snapshot.apply(AdapterEvent::TopicUpsert {
            topic: forum_row("telegram:9", food, "Food", 0, 1),
        });
        let message = |topic, id| {
            let mut row = telegram_text(9, id, "older");
            row.topic = Some(topic);
            row
        };
        snapshot.apply(AdapterEvent::MessageReceived {
            message: message(trail, 50),
        });
        snapshot.apply(AdapterEvent::MessageReceived {
            message: message(food, 40),
        });
        snapshot.select_conversation("telegram:9".into());
        snapshot.select_topic("telegram:9".into(), trail);
        snapshot.apply(AdapterEvent::HistoryLoaded {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:9".into(),
            store_cursor: None,
        });
        snapshot.take_commands();
        snapshot.load_older();
        assert_eq!(snapshot.older_state(), OlderState::Loading);
        snapshot.select_topic("telegram:9".into(), food);
        snapshot.apply(AdapterEvent::HistoryLoaded {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:9".into(),
            store_cursor: None,
        });
        snapshot.take_commands();
        assert_eq!(snapshot.older_state(), OlderState::Idle);
        snapshot.apply(AdapterEvent::OlderHistoryLoaded {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:9".into(),
            before_message_id: "telegram:9:50".into(),
            more: false,
            note: None,
            topic: Some(trail),
            store_cursor: None,
        });
        assert_eq!(
            snapshot.older_state(),
            OlderState::Idle,
            "Food is not at the start"
        );
        assert!(snapshot.older_can_ask());
        snapshot.load_older();
        assert!(snapshot.take_commands().iter().any(|command| matches!(
            command,
            AdapterCommand::LoadOlderTopic { topic, before_message_id, .. }
                if *topic == food && before_message_id == "telegram:9:40"
        )));
        snapshot.select_topic("telegram:9".into(), trail);
        snapshot.apply(AdapterEvent::HistoryLoaded {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:9".into(),
            store_cursor: None,
        });
        assert_eq!(snapshot.older_state(), OlderState::StartOfChat);
        let _ = store;
    }

    /// A send from topic A, then a switch to topic B, still keeps A's text
    /// on A's draft when the session ends.
    #[test]
    fn a_pending_topic_send_is_kept_on_that_topic() {
        let mut snapshot = shell_with(&[ProtocolId::Discord]);
        link(&mut snapshot, ProtocolId::Discord);
        allow_send(&mut snapshot, ProtocolId::Discord);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Discord, "discord:1", true),
        });
        let topic = |id, name: &str| ChatTopic {
            protocol: ProtocolId::Discord,
            conversation_id: "discord:1".into(),
            topic: MessageTopic::Forum(id),
            name: name.into(),
            unread: 0,
            last_at: id,
            general: false,
            hidden: false,
            closed: false,
            muted: false,
        };
        snapshot.apply(AdapterEvent::TopicUpsert {
            topic: topic(1, "One"),
        });
        snapshot.apply(AdapterEvent::TopicUpsert {
            topic: topic(2, "Two"),
        });
        snapshot.select_protocol(ProtocolId::Discord);
        snapshot.select_conversation("discord:1".into());
        snapshot.select_topic("discord:1".into(), MessageTopic::Forum(1));
        snapshot.compose = "ok".into();
        snapshot.send_compose();
        snapshot.select_topic("discord:1".into(), MessageTopic::Forum(2));
        snapshot.compose = "later".into();
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Discord,
            state: AccountState::Unlinked,
        });
        assert_eq!(
            snapshot
                .drafts
                .get(&(
                    ProtocolId::Discord,
                    "discord:1".into(),
                    Some(MessageTopic::Forum(1))
                ))
                .map(String::as_str),
            Some("ok")
        );
        assert!(
            !snapshot
                .drafts
                .contains_key(&(ProtocolId::Discord, "discord:1".into(), None)),
            "the text stays on the topic"
        );
    }

    #[test]
    fn a_reorder_does_not_scroll_the_highlight() {
        let store = SecretStore::memory();
        let mut snapshot = Snapshot::new();
        complete_telegram(&mut snapshot, &store);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: telegram_chat(1, "Ada", 10),
        });
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: telegram_chat(2, "Bob", 5),
        });
        snapshot.move_inbox_selection(1);
        assert_eq!(snapshot.focused_row.as_deref(), Some("telegram:2"));
        assert!(snapshot.take_scroll_to_focused());

        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: telegram_chat(1, "Ada", 11),
        });
        assert!(
            !snapshot.wants_scroll_to_focused(),
            "the highlighted row did not move"
        );

        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: telegram_chat(3, "Cy", 7),
        });
        assert_eq!(snapshot.focused_row.as_deref(), Some("telegram:2"));
        assert!(
            !snapshot.wants_scroll_to_focused(),
            "a row inserted above the highlight does not scroll"
        );
    }

    #[test]
    fn a_search_scrolls_the_highlight_when_its_visible_place_changes() {
        let store = SecretStore::memory();
        let mut snapshot = Snapshot::new();
        complete_telegram(&mut snapshot, &store);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: telegram_chat(1, "Ada", 30),
        });
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: telegram_chat(2, "Bob", 20),
        });
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: telegram_chat(3, "Cara", 10),
        });
        snapshot.set_search("a".into());
        snapshot.move_inbox_selection(1);
        assert_eq!(snapshot.focused_row.as_deref(), Some("telegram:3"));
        assert_eq!(visible_place(&snapshot, "telegram:3"), Some(1));
        assert!(snapshot.take_scroll_to_focused());

        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: telegram_chat(2, "Bea", 20),
        });
        assert_eq!(
            visible_place(&snapshot, "telegram:3"),
            Some(2),
            "Bea joins the filtered list above Cara"
        );
        assert!(
            !snapshot.wants_scroll_to_focused(),
            "a row update does not scroll the highlight"
        );
    }

    #[test]
    fn a_highlight_scrolls_only_when_its_place_changes() {
        let store = SecretStore::memory();
        let mut snapshot = Snapshot::new();
        complete_telegram(&mut snapshot, &store);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: telegram_chat(1, "Ada", 30),
        });
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: telegram_chat(2, "Bob", 20),
        });
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: telegram_chat(3, "Cara", 10),
        });
        snapshot.move_inbox_selection(-1);
        assert_eq!(snapshot.focused_row.as_deref(), Some("telegram:1"));
        assert!(snapshot.take_scroll_to_focused());

        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: telegram_chat(4, "Dee", 1),
        });
        assert!(
            !snapshot.wants_scroll_to_focused(),
            "a row below the highlight does not scroll"
        );

        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: telegram_chat(1, "Ada", 5),
        });
        assert_eq!(snapshot.focused_row.as_deref(), Some("telegram:1"));
        assert!(
            !snapshot.wants_scroll_to_focused(),
            "a resort that moves the highlight does not scroll"
        );

        snapshot.set_search("Ada".into());
        assert!(snapshot.take_scroll_to_focused());
        snapshot.set_search(String::new());
        assert_eq!(snapshot.focused_row.as_deref(), Some("telegram:1"));
        assert!(
            snapshot.take_scroll_to_focused(),
            "clearing a search scrolls the highlight"
        );
    }

    fn visible_place(snapshot: &Snapshot, id: &str) -> Option<usize> {
        snapshot
            .visible_conversations()
            .iter()
            .position(|row| row.id == id)
    }

    fn outgoing(chat: i64, id: i64, body: &str, delivery: Delivery) -> ChatMessage {
        ChatMessage {
            protocol: ProtocolId::Telegram,
            conversation_id: format!("telegram:{chat}"),
            id: format!("telegram:{chat}:{id}"),
            sender: "you".into(),
            body: body.into(),
            outbound: true,
            delivery,
            sent_at: 0,
            arrival: thinwire_protocol::Arrival::History,
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

    fn older_requests(snapshot: &mut Snapshot) -> Vec<String> {
        snapshot
            .take_commands()
            .into_iter()
            .filter_map(|command| match command {
                AdapterCommand::LoadOlderMessages {
                    before_message_id, ..
                } => Some(before_message_id),
                _ => None,
            })
            .collect()
    }

    fn older_loaded(chat: i64, before: i64, more: bool) -> AdapterEvent {
        AdapterEvent::OlderHistoryLoaded {
            protocol: ProtocolId::Telegram,
            conversation_id: format!("telegram:{chat}"),
            before_message_id: format!("telegram:{chat}:{before}"),
            more,
            note: None,
            topic: None,
            store_cursor: None,
        }
    }

    /// A chat with messages 50 and 51 loaded, and its first page done.
    fn chat_with_recent_page(store: &SecretStore) -> Snapshot {
        let mut snapshot = ready_with_chats(store);
        assert_eq!(
            snapshot.selected_conversation.as_deref(),
            Some("telegram:1")
        );
        for id in [51, 50] {
            snapshot.apply(AdapterEvent::MessageReceived {
                message: outgoing(1, id, "recent", Delivery::Sent),
            });
        }
        snapshot.apply(AdapterEvent::HistoryLoaded {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            store_cursor: None,
        });
        snapshot.take_commands();
        snapshot
    }

    /// A chat muted in the protocol or in thinwire (#153) does not count.
    #[test]
    fn unread_total_skips_muted_chats_and_protocols_without_a_session() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        let chat = |id: i64, unread: u32, muted: bool| Conversation {
            unread,
            muted,
            ..telegram_chat(id, "Chat", 10 - id)
        };
        for row in [chat(1, 3, false), chat(2, 4, true), chat(3, 5, false)] {
            snapshot.apply(AdapterEvent::ConversationUpsert { conversation: row });
        }
        let mut mutes = ChatMutes::in_memory();
        assert_eq!(
            snapshot.unread_total(&mutes),
            8,
            "the muted chat does not count"
        );
        assert!(
            snapshot
                .conversation(ProtocolId::Telegram, "telegram:2")
                .is_some_and(|row| row.muted)
        );
        mutes.set(ProtocolId::Telegram, "telegram:3", true);
        assert_eq!(
            snapshot.unread_total(&mutes),
            3,
            "a chat muted in thinwire does not count"
        );
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Telegram,
            state: AccountState::Unlinked,
        });
        assert_eq!(snapshot.unread_total(&mutes), 0, "no session, no count");
    }

    /// A click on a reply quote whose message is loaded scrolls to it at
    /// once, and asks for no page.
    #[test]
    fn a_reply_jump_to_a_loaded_message_scrolls_to_it() {
        let store = SecretStore::memory();
        let mut snapshot = chat_with_recent_page(&store);
        snapshot.jump_to_message("telegram:1:50");
        assert_eq!(snapshot.wants_scroll_to_message(), Some("telegram:1:50"));
        assert!(older_requests(&mut snapshot).is_empty());
        assert_eq!(
            snapshot.take_scroll_to_message().as_deref(),
            Some("telegram:1:50")
        );
        assert_eq!(
            snapshot.wants_scroll_to_message(),
            None,
            "one scroll per click"
        );
    }

    /// The replied message is not loaded: older pages load until it arrives,
    /// then the thread scrolls to it.
    #[test]
    fn a_reply_jump_loads_older_pages_until_the_message_arrives() {
        let store = SecretStore::memory();
        let mut snapshot = chat_with_recent_page(&store);
        snapshot.jump_to_message("telegram:1:30");
        assert_eq!(older_requests(&mut snapshot), vec!["telegram:1:50"]);
        assert_eq!(snapshot.wants_scroll_to_message(), None);

        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(1, 40, "older", Delivery::Sent),
        });
        snapshot.apply(older_loaded(1, 50, true));
        assert_eq!(
            older_requests(&mut snapshot),
            vec!["telegram:1:40"],
            "the next page goes out by itself"
        );

        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(1, 30, "the question", Delivery::Sent),
        });
        snapshot.apply(older_loaded(1, 40, true));
        assert_eq!(snapshot.wants_scroll_to_message(), Some("telegram:1:30"));
        assert!(older_requests(&mut snapshot).is_empty());
    }

    /// The replied message never arrives: the jump stops after
    /// `REPLY_JUMP_PAGES` pages and does not loop.
    #[test]
    fn a_reply_jump_stops_after_its_page_budget() {
        let store = SecretStore::memory();
        let mut snapshot = chat_with_recent_page(&store);
        snapshot.jump_to_message("telegram:1:1");
        let mut pages = 0;
        let mut oldest = 50;
        while let [anchor] = older_requests(&mut snapshot).as_slice() {
            assert_eq!(anchor, &format!("telegram:1:{oldest}"));
            pages += 1;
            let older = oldest - 2;
            snapshot.apply(AdapterEvent::MessageReceived {
                message: outgoing(1, older, "older", Delivery::Sent),
            });
            snapshot.apply(older_loaded(1, oldest, true));
            oldest = older;
        }
        assert_eq!(pages, usize::from(REPLY_JUMP_PAGES));
        assert_eq!(snapshot.wants_scroll_to_message(), None);
    }

    /// Another chat opens while pages load: the jump ends there.
    #[test]
    fn a_reply_jump_ends_when_the_chat_changes() {
        let store = SecretStore::memory();
        let mut snapshot = chat_with_recent_page(&store);
        snapshot.jump_to_message("telegram:1:30");
        assert_eq!(older_requests(&mut snapshot).len(), 1);
        snapshot.select_conversation("telegram:2".into());
        snapshot.select_conversation("telegram:1".into());
        snapshot.take_commands();
        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(1, 30, "the question", Delivery::Sent),
        });
        snapshot.apply(older_loaded(1, 50, true));
        assert_eq!(snapshot.wants_scroll_to_message(), None);
        assert!(older_requests(&mut snapshot).is_empty());
    }

    #[test]
    fn older_pages_merge_above_and_stop_at_the_start_of_the_chat() {
        let store = SecretStore::memory();
        let mut snapshot = chat_with_recent_page(&store);
        assert_eq!(snapshot.older_state(), OlderState::Idle);

        snapshot.load_older();
        assert_eq!(older_requests(&mut snapshot), vec!["telegram:1:50"]);
        assert_eq!(snapshot.older_state(), OlderState::Loading);
        snapshot.load_older();
        assert!(
            older_requests(&mut snapshot).is_empty(),
            "one request at a time"
        );

        // The older page arrives newest first; it merges above, oldest first.
        for id in [42, 40] {
            snapshot.apply(AdapterEvent::MessageReceived {
                message: outgoing(1, id, "older", Delivery::Sent),
            });
        }
        snapshot.apply(older_loaded(1, 50, true));
        let ids: Vec<&str> = snapshot
            .selected_messages()
            .iter()
            .map(|row| row.id.as_str())
            .collect();
        assert_eq!(
            ids,
            [
                "telegram:1:40",
                "telegram:1:42",
                "telegram:1:50",
                "telegram:1:51"
            ]
        );
        assert_eq!(snapshot.older_state(), OlderState::Idle);

        snapshot.load_older();
        assert_eq!(older_requests(&mut snapshot), vec!["telegram:1:40"]);
        snapshot.apply(older_loaded(1, 40, false));
        assert_eq!(snapshot.older_state(), OlderState::StartOfChat);
        snapshot.load_older();
        assert!(
            older_requests(&mut snapshot).is_empty(),
            "no request after the start"
        );

        // Another chat pages on its own.
        snapshot.select_conversation("telegram:2".into());
        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(2, 9, "bob", Delivery::Sent),
        });
        snapshot.apply(AdapterEvent::HistoryLoaded {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:2".into(),
            store_cursor: None,
        });
        snapshot.take_commands();
        assert_eq!(snapshot.older_state(), OlderState::Idle);
        snapshot.load_older();
        assert_eq!(older_requests(&mut snapshot), vec!["telegram:2:9"]);
    }

    #[test]
    fn a_telegram_error_line_comes_back_after_an_older_page() {
        let store = SecretStore::memory();
        let mut snapshot = chat_with_recent_page(&store);
        snapshot.apply(AdapterEvent::ChatListLoaded {
            protocol: ProtocolId::Telegram,
        });
        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Telegram,
            status: AdapterStatus::Error,
            detail: "Could not mark messages read.".into(),
        });
        assert_eq!(snapshot.status_line(), "Could not mark messages read.");

        // Scroll up: the strip shows the older page's loading line.
        snapshot.load_older();
        assert_eq!(snapshot.status_line(), LOADING_OLDER_STATUS);

        // The page ends: the error line is back, not an empty line (qa).
        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(1, 40, "older", Delivery::Sent),
        });
        snapshot.apply(older_loaded(1, 50, true));
        assert_eq!(snapshot.status_line(), "Could not mark messages read.");
        assert_eq!(snapshot.status_text, "Could not mark messages read.");
    }

    #[test]
    fn an_older_page_failure_shows_a_chat_note_until_a_success() {
        let store = SecretStore::memory();
        let mut snapshot = chat_with_recent_page(&store);
        snapshot.apply(AdapterEvent::ChatListLoaded {
            protocol: ProtocolId::Telegram,
        });
        assert!(!snapshot.is_loading());
        snapshot.load_older();
        assert!(snapshot.is_loading(), "an older page in flight is a load");

        let failed = AdapterEvent::OlderHistoryLoaded {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            before_message_id: "telegram:1:50".into(),
            more: true,
            note: Some("Could not load older messages (TDLib 500).".into()),
            topic: None,
            store_cursor: None,
        };
        snapshot.apply(failed);
        assert!(!snapshot.is_loading());
        // Plain text: no library name and no code (qa on #73).
        assert_eq!(snapshot.older_note(), Some(OLDER_PAGE_FAILED_NOTE));
        let note = snapshot.older_note().unwrap_or_default();
        assert!(!note.contains("TDLib") && !note.chars().any(|c| c.is_ascii_digit()));
        assert!(
            snapshot.error.is_none(),
            "a chat note, not an account error"
        );
        // Other chats do not show it.
        snapshot.select_conversation("telegram:2".into());
        assert_eq!(snapshot.older_note(), None);
        snapshot.select_conversation("telegram:1".into());

        // A later success clears it.
        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(1, 40, "older", Delivery::Sent),
        });
        snapshot.apply(older_loaded(1, 50, true));
        assert_eq!(snapshot.older_note(), None);
    }

    #[test]
    fn older_notes_belong_to_one_protocol_and_leave_with_its_session() {
        let mut snapshot = Snapshot::new();
        let failed = |protocol: ProtocolId, chat: &str| AdapterEvent::OlderHistoryLoaded {
            protocol,
            conversation_id: chat.into(),
            before_message_id: format!("{chat}:9"),
            more: true,
            note: Some("group-7f3a is not in the local store".into()),
            topic: None,
            store_cursor: None,
        };
        for protocol in [ProtocolId::Signal, ProtocolId::Telegram] {
            snapshot.apply(AdapterEvent::Account {
                protocol,
                state: AccountState::Linked,
            });
        }
        // The same chat id in two protocols.
        snapshot.apply(failed(ProtocolId::Signal, "chat:1"));
        snapshot.apply(failed(ProtocolId::Telegram, "chat:1"));
        snapshot.selected_protocol = ProtocolId::Signal;
        snapshot.selected_conversation = Some("chat:1".into());
        assert_eq!(
            snapshot.older_note(),
            Some(OLDER_PAGE_FAILED_NOTE),
            "Signal too"
        );

        snapshot.end_session(ProtocolId::Signal);
        snapshot.selected_protocol = ProtocolId::Signal;
        snapshot.selected_conversation = Some("chat:1".into());
        assert_eq!(snapshot.older_note(), None, "the Signal session ended");
        snapshot.selected_protocol = ProtocolId::Telegram;
        assert_eq!(
            snapshot.older_note(),
            Some(OLDER_PAGE_FAILED_NOTE),
            "the Telegram note stays"
        );
    }

    #[test]
    fn ending_a_signal_session_clears_paging_so_a_relink_can_ask() {
        let mut snapshot = Snapshot::new();
        let chat = "signal-chat";
        let anchor = "1700000000000:ada";
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Signal,
            state: AccountState::Linked,
        });
        snapshot.selected_protocol = ProtocolId::Signal;
        snapshot.selected_conversation = Some(chat.into());
        snapshot.apply(AdapterEvent::MessageReceived {
            message: ChatMessage {
                protocol: ProtocolId::Signal,
                conversation_id: chat.into(),
                id: anchor.into(),
                sender: "Ada".into(),
                body: "hello".into(),
                outbound: false,
                delivery: Delivery::Sent,
                sent_at: 1,
                arrival: thinwire_protocol::Arrival::History,
                topic: None,
                links: Vec::new(),
                photo: None,
                audio: None,
                video: None,
                file: None,
                reactions: Vec::new(),
                reply_to: None,
                edited: false,
            },
        });
        snapshot.apply(AdapterEvent::OlderHistoryLoaded {
            protocol: ProtocolId::Signal,
            conversation_id: chat.into(),
            before_message_id: anchor.into(),
            more: false,
            note: None,
            topic: None,
            store_cursor: None,
        });
        assert_eq!(snapshot.older_state(), OlderState::StartOfChat);
        assert!(!snapshot.older_can_ask());

        snapshot.end_session(ProtocolId::Signal);
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Signal,
            state: AccountState::Linked,
        });
        snapshot.selected_protocol = ProtocolId::Signal;
        snapshot.selected_conversation = Some(chat.into());
        snapshot.apply(AdapterEvent::MessageReceived {
            message: ChatMessage {
                protocol: ProtocolId::Signal,
                conversation_id: chat.into(),
                id: anchor.into(),
                sender: "Ada".into(),
                body: "hello".into(),
                outbound: false,
                delivery: Delivery::Sent,
                sent_at: 1,
                arrival: thinwire_protocol::Arrival::History,
                topic: None,
                links: Vec::new(),
                photo: None,
                audio: None,
                video: None,
                file: None,
                reactions: Vec::new(),
                reply_to: None,
                edited: false,
            },
        });
        assert_eq!(snapshot.older_state(), OlderState::Idle);
        assert!(snapshot.older_can_ask(), "a relink can page again");
    }

    #[test]
    fn an_open_signal_chat_does_not_count_unread() {
        let mut snapshot = Snapshot::new();
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Signal,
            state: AccountState::Linked,
        });
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: Conversation {
                protocol: ProtocolId::Signal,
                id: "s1".into(),
                title: "Ada".into(),
                participant: "s1".into(),
                preview: "hello".into(),
                unread: 1,
                order: 1,
                last_at: 1,
                is_group: false,
                writable: true,
                muted: false,
                placeholder: false,
                has_topics: false,
                avatar: Default::default(),
            },
        });
        assert_eq!(snapshot.unread_for(ProtocolId::Signal), 1);
        snapshot.extra_visible.insert(ProtocolId::Signal);
        snapshot.selected_protocol = ProtocolId::Signal;
        snapshot.select_conversation("s1".into());
        snapshot.sync_viewed();
        assert_eq!(snapshot.unread_for(ProtocolId::Signal), 0);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: Conversation {
                protocol: ProtocolId::Signal,
                id: "s1".into(),
                title: "Ada".into(),
                participant: "s1".into(),
                preview: "again".into(),
                unread: 4,
                order: 2,
                last_at: 2,
                is_group: false,
                writable: true,
                muted: false,
                placeholder: false,
                has_topics: false,
                avatar: Default::default(),
            },
        });
        assert_eq!(snapshot.unread_for(ProtocolId::Signal), 0);
        assert_eq!(
            snapshot
                .selected_conversation_row()
                .map(|row| row.preview.as_str()),
            Some("again")
        );
    }

    #[test]
    fn an_older_request_that_brought_nothing_waits_before_the_same_anchor() {
        let store = SecretStore::memory();
        let mut snapshot = chat_with_recent_page(&store);
        let start = Instant::now();
        snapshot.load_older_at(start);
        assert_eq!(older_requests(&mut snapshot), vec!["telegram:1:50"]);
        // A failure or an anchor-only page: `more`, but nothing older came.
        snapshot.apply(older_loaded(1, 50, true));
        assert_eq!(snapshot.older_state(), OlderState::Idle);

        let ended = Instant::now();
        snapshot.load_older_at(ended);
        snapshot.load_older_at(ended + OLDER_RETRY_DELAY / 2);
        assert!(
            older_requests(&mut snapshot).is_empty(),
            "no loop on the same anchor"
        );
        snapshot.load_older_at(ended + OLDER_RETRY_DELAY);
        assert_eq!(
            older_requests(&mut snapshot),
            vec!["telegram:1:50"],
            "after the delay, one more try"
        );

        // A page that brought older rows clears the wait.
        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(1, 40, "older", Delivery::Sent),
        });
        snapshot.apply(older_loaded(1, 50, true));
        snapshot.load_older_at(Instant::now());
        assert_eq!(older_requests(&mut snapshot), vec!["telegram:1:40"]);
    }

    #[test]
    fn a_short_thread_asks_again_after_the_wait_and_backs_off() {
        let store = SecretStore::memory();
        let mut snapshot = chat_with_recent_page(&store);
        // Anchor-only page: `more`, and nothing older came.
        snapshot.load_older();
        assert_eq!(older_requests(&mut snapshot).len(), 1);
        snapshot.apply(older_loaded(1, 50, true));
        let ended = Instant::now();
        assert!(!snapshot.older_can_ask(), "no ask during the wait");
        snapshot.load_older_at(ended + OLDER_RETRY_DELAY / 2);
        assert!(older_requests(&mut snapshot).is_empty());
        // After the wait: one more request, with no scroll gesture.
        snapshot.load_older_at(ended + OLDER_RETRY_DELAY);
        assert_eq!(older_requests(&mut snapshot), vec!["telegram:1:50"]);
        snapshot.load_older_at(ended + OLDER_RETRY_DELAY);
        assert!(older_requests(&mut snapshot).is_empty(), "one at a time");

        // Nothing again: the wait doubles, so there is no fast loop.
        snapshot.apply(older_loaded(1, 50, true));
        let again = Instant::now();
        snapshot.load_older_at(again + OLDER_RETRY_DELAY);
        assert!(older_requests(&mut snapshot).is_empty());
        snapshot.load_older_at(again + OLDER_RETRY_DELAY * 2);
        assert_eq!(older_requests(&mut snapshot).len(), 1);
        assert_eq!(older_wait(1), OLDER_RETRY_DELAY);
        assert_eq!(older_wait(30), OLDER_RETRY_MAX, "capped");

        // The start of the chat: no more asks.
        snapshot.apply(older_loaded(1, 50, false));
        assert!(!snapshot.older_can_ask());
        snapshot.load_older_at(again + OLDER_RETRY_MAX * 2);
        assert!(older_requests(&mut snapshot).is_empty());
    }

    #[test]
    fn no_older_request_before_the_first_page_or_after_a_failure_stops_it() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        snapshot.take_commands();
        // The first page of telegram:1 is still loading.
        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(1, 50, "recent", Delivery::Sent),
        });
        snapshot.load_older();
        assert!(older_requests(&mut snapshot).is_empty());

        let mut snapshot = chat_with_recent_page(&store);
        snapshot.load_older();
        assert_eq!(older_requests(&mut snapshot).len(), 1);
        // A failed load sends no end event: the error status stops the row.
        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Telegram,
            status: AdapterStatus::Error,
            detail: "history failed".into(),
        });
        assert_eq!(snapshot.older_state(), OlderState::Idle);

        // A removed chat forgets its paging.
        snapshot.load_older();
        snapshot.apply(older_loaded(1, 50, false));
        assert_eq!(snapshot.older_state(), OlderState::StartOfChat);
        snapshot.apply(AdapterEvent::ConversationRemoved {
            protocol: ProtocolId::Telegram,
            id: "telegram:1".into(),
        });
        assert!(!snapshot.older_at_start.contains(&(
            ProtocolId::Telegram,
            "telegram:1".to_owned(),
            None
        )));
    }

    fn send_texts(snapshot: &mut Snapshot) -> Vec<String> {
        snapshot
            .take_commands()
            .into_iter()
            .filter_map(|command| match command {
                AdapterCommand::SendText { body, .. } => Some(body),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn enter_sends_and_shift_enter_keeps_the_line() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        snapshot.compose = "line one".into();
        assert!(
            !snapshot.compose_enter(true),
            "Shift+Enter goes to the text field"
        );
        assert_eq!(snapshot.compose, "line one");
        assert!(send_texts(&mut snapshot).is_empty());

        snapshot.compose = "line one\nline two".into();
        assert!(snapshot.compose_enter(false));
        assert_eq!(send_texts(&mut snapshot), vec!["line one\nline two"]);
        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(1, 100, "line one\nline two", Delivery::Pending),
        });
        accept(&mut snapshot, "telegram:1");
        assert!(
            snapshot.compose.is_empty(),
            "cleared once the send is accepted"
        );

        snapshot.compose = "   ".into();
        assert!(!snapshot.can_send());
        assert!(
            snapshot.compose_enter(false),
            "plain Enter never adds a line"
        );
        assert!(send_texts(&mut snapshot).is_empty());
        assert!(snapshot.error.is_none());
    }

    #[test]
    fn arrow_moves_the_inbox_and_enter_opens_the_chat() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        assert_eq!(
            snapshot.selected_conversation.as_deref(),
            Some("telegram:1")
        );
        snapshot.compose = "draft for Ada".into();
        snapshot.move_inbox_selection(1);
        assert_eq!(
            snapshot.selected_conversation.as_deref(),
            Some("telegram:1"),
            "an arrow does not replace the open chat"
        );
        assert_eq!(snapshot.compose, "draft for Ada");
        assert_eq!(snapshot.focused_row.as_deref(), Some("telegram:2"));
        assert!(snapshot.take_scroll_to_focused());
        assert!(!snapshot.wants_focus_compose());
        assert!(
            snapshot.take_commands().is_empty(),
            "an arrow does not open the chat"
        );
        snapshot.move_inbox_selection(1);
        assert_eq!(snapshot.focused_row.as_deref(), Some("telegram:2"));
        assert!(
            !snapshot.wants_scroll_to_focused(),
            "the last row does not request another scroll"
        );
        snapshot.move_inbox_selection(-1);
        assert_eq!(snapshot.focused_row.as_deref(), Some("telegram:1"));
        assert!(snapshot.take_scroll_to_focused());
        assert_eq!(
            snapshot.selected_conversation.as_deref(),
            Some("telegram:1")
        );
        snapshot.select_conversation("telegram:2".into());
        assert!(snapshot.take_focus_compose());
        assert!(
            snapshot
                .take_commands()
                .iter()
                .any(|command| matches!(command, AdapterCommand::OpenChat { .. })),
            "Enter opens the highlighted chat"
        );
    }

    #[test]
    fn a_hidden_highlight_is_cleared_and_enter_opens_nothing() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        snapshot.move_inbox_selection(1);
        assert_eq!(snapshot.focused_row.as_deref(), Some("telegram:2"));
        snapshot.set_search("Ada".into());
        assert!(
            snapshot.focused_row.is_none(),
            "the hidden row is not highlighted"
        );
        assert!(snapshot.visible_focused_row().is_none());
        assert_eq!(
            snapshot.selected_conversation.as_deref(),
            Some("telegram:1")
        );
        assert!(snapshot.take_commands().is_empty());

        snapshot.set_search(String::new());
        snapshot.move_inbox_selection(1);
        snapshot.set_search("Bob".into());
        assert_eq!(
            snapshot.visible_focused_row().as_deref(),
            Some("telegram:2"),
            "a row that still matches stays highlighted"
        );

        snapshot.apply(AdapterEvent::ConversationRemoved {
            protocol: ProtocolId::Telegram,
            id: "telegram:2".into(),
        });
        assert!(snapshot.focused_row.is_none());
        assert!(snapshot.visible_focused_row().is_none());
    }

    #[test]
    fn picking_a_chat_focuses_compose_and_keeps_drafts_per_chat() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        assert_eq!(
            snapshot.selected_conversation.as_deref(),
            Some("telegram:1")
        );
        assert!(
            !snapshot.take_focus_compose(),
            "auto-select does not steal focus"
        );
        snapshot.compose = "draft for Ada".into();
        snapshot.select_conversation("telegram:2".into());
        assert!(snapshot.take_focus_compose());
        assert!(!snapshot.take_focus_compose());
        assert_eq!(snapshot.compose, "");
        snapshot.compose = "draft for Bob".into();
        snapshot.select_conversation("telegram:1".into());
        assert_eq!(snapshot.compose, "draft for Ada");
        snapshot.select_conversation("telegram:2".into());
        assert_eq!(snapshot.compose, "draft for Bob");
        assert!(
            send_texts(&mut snapshot).is_empty(),
            "switching never sends"
        );
    }

    #[test]
    fn a_removed_chat_that_is_not_selected_loses_its_draft() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        snapshot.compose = "draft for Ada".into();
        snapshot.select_conversation("telegram:2".into());
        snapshot.compose = "draft for Bob".into();
        assert!(
            snapshot
                .drafts
                .contains_key(&(ProtocolId::Telegram, "telegram:1".into(), None))
        );

        snapshot.apply(AdapterEvent::ConversationRemoved {
            protocol: ProtocolId::Telegram,
            id: "telegram:1".into(),
        });
        assert!(
            !snapshot
                .drafts
                .contains_key(&(ProtocolId::Telegram, "telegram:1".into(), None)),
            "draft gone"
        );
        assert_eq!(
            snapshot.selected_conversation.as_deref(),
            Some("telegram:2"),
            "the selection stays"
        );
        assert_eq!(snapshot.compose, "draft for Bob", "the open draft stays");
    }

    #[test]
    fn pending_send_turns_sent_on_success() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(1, 100, "hi", Delivery::Pending),
        });
        assert_eq!(snapshot.selected_messages()[0].delivery, Delivery::Pending);
        snapshot.apply(AdapterEvent::MessageReplaced {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            old_id: "telegram:1:100".into(),
            message: outgoing(1, 200, "hi", Delivery::Sent),
        });
        let messages = snapshot.selected_messages();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].id, "telegram:1:200");
        assert_eq!(messages[0].delivery, Delivery::Sent);
        assert!(snapshot.error.is_none());
    }

    /// Defensive. `td_api.tl` calls the succeeded message the sent message.
    /// Main maps a null `sending_state` to `Sent`. This case is the extra
    /// path: the succeeded object is still pending, and the update still wins.
    #[test]
    fn an_outgoing_telegram_message_follows_the_tdlib_send_sequence() {
        use thinwire_protocol::{OutgoingUpdate, outgoing_delivery};

        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        let _request = pending_telegram_send(&mut snapshot, 1, 100, "hi");
        assert_eq!(snapshot.status_line(), SENDING_STATUS, "pending");
        let pending = snapshot
            .selected_messages()
            .iter()
            .find(|row| row.body == "hi")
            .expect("pending row");
        assert_eq!(pending.id, "telegram:1:100", "pending");
        assert_eq!(pending.delivery, Delivery::Pending, "pending");
        // updateMessageSendAcknowledged does not replace the temporary id.
        assert_eq!(
            snapshot
                .selected_messages()
                .iter()
                .filter(|row| row.body == "hi")
                .count(),
            1,
            "acknowledged"
        );

        // Defensive fixture: sending_state is still Pending on the succeeded message.
        let server = 1_048_576;
        let delivery = outgoing_delivery(
            OutgoingUpdate::Succeeded,
            Delivery::Pending,
            true,
            server,
            0,
        );
        assert_eq!(delivery, Delivery::Sent, "succeeded");
        snapshot.apply(AdapterEvent::MessageReplaced {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            old_id: "telegram:1:100".into(),
            message: outgoing(1, server, "hi", delivery),
        });
        let hi: Vec<_> = snapshot
            .selected_messages()
            .iter()
            .filter(|row| row.body == "hi")
            .collect();
        assert_eq!(hi.len(), 1, "the temporary id is not a second row");
        assert_eq!(hi[0].id, format!("telegram:1:{server}"));
        assert_eq!(hi[0].delivery, Delivery::Sent);
        assert!(
            snapshot
                .selected_messages()
                .iter()
                .all(|row| row.id != "telegram:1:100")
        );
        assert_ne!(snapshot.status_line(), SENDING_STATUS, "succeeded");

        snapshot.apply(AdapterEvent::OutboxRead {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            last_read_message_id: server,
        });
        assert_eq!(
            snapshot
                .selected_messages()
                .iter()
                .find(|row| row.body == "hi")
                .map(|row| row.delivery),
            Some(Delivery::Read),
            "read outbox"
        );

        // A chat load carries the same cursor. A later sent id stays sent.
        // A pending local id below the cursor stays pending.
        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(1, 20, "old", Delivery::Sent),
        });
        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(1, server + 1_048_576, "newer", Delivery::Sent),
        });
        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(1, 7, "still sending", Delivery::Pending),
        });
        let by_id = |id: i64| {
            snapshot
                .selected_messages()
                .iter()
                .find(|row| row.id == format!("telegram:1:{id}"))
                .map(|row| row.delivery)
        };
        assert_eq!(by_id(20), Some(Delivery::Read), "read outbox");
        assert_eq!(
            by_id(server + 1_048_576),
            Some(Delivery::Sent),
            "read outbox"
        );
        assert_eq!(by_id(7), Some(Delivery::Pending), "read outbox");
    }

    /// Incoming and outgoing Telegram rows share one message id space.
    /// The outbox cursor marks only an outbound row as Read.
    #[test]
    fn an_outbox_cursor_does_not_mark_an_incoming_row_read() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        let cursor = 50;

        snapshot.apply(AdapterEvent::MessageReceived {
            message: telegram_text(1, 10, "from Ada"),
        });
        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(1, 20, "to Ada", Delivery::Sent),
        });
        snapshot.apply(AdapterEvent::OutboxRead {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            last_read_message_id: cursor,
        });
        let delivery = |snapshot: &Snapshot, id: i64| {
            snapshot
                .selected_messages()
                .iter()
                .find(|row| row.id == format!("telegram:1:{id}"))
                .map(|row| row.delivery)
        };
        assert_eq!(
            delivery(&snapshot, 10),
            Some(Delivery::Sent),
            "an incoming row already loaded stays sent"
        );
        assert_eq!(
            delivery(&snapshot, 20),
            Some(Delivery::Read),
            "an outbound row already loaded becomes read"
        );

        snapshot.apply(AdapterEvent::MessageReceived {
            message: telegram_text(1, 30, "later from Ada"),
        });
        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(1, 40, "later to Ada", Delivery::Sent),
        });
        assert_eq!(
            delivery(&snapshot, 30),
            Some(Delivery::Sent),
            "an incoming row loaded after the cursor stays sent"
        );
        assert_eq!(
            delivery(&snapshot, 40),
            Some(Delivery::Read),
            "an outbound row loaded after the cursor becomes read"
        );

        // A delivery update uses the same rule.
        snapshot.apply(AdapterEvent::MessageReceived {
            message: ChatMessage {
                delivery: Delivery::Pending,
                ..telegram_text(1, 12, "from Ada pending")
            },
        });
        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(1, 14, "to Ada pending", Delivery::Pending),
        });
        snapshot.apply(AdapterEvent::MessageDelivery {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            message_id: "telegram:1:12".into(),
            delivery: Delivery::Sent,
        });
        snapshot.apply(AdapterEvent::MessageDelivery {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            message_id: "telegram:1:14".into(),
            delivery: Delivery::Sent,
        });
        assert_eq!(
            delivery(&snapshot, 12),
            Some(Delivery::Sent),
            "an incoming delivery update stays sent"
        );
        assert_eq!(
            delivery(&snapshot, 14),
            Some(Delivery::Read),
            "an outbound delivery update becomes read"
        );
    }

    /// Defensive. Main maps `messageSendingStateFailed` to `Failed`.
    /// This case still checks a pending object on `updateMessageSendFailed`.
    #[test]
    fn a_failed_telegram_send_shows_not_sent_and_queues_retry() {
        use thinwire_protocol::{OutgoingUpdate, outgoing_delivery};

        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        let _request = pending_telegram_send(&mut snapshot, 1, 100, "hi");
        // Defensive fixture: sending_state is still Pending on the failed message.
        let delivery = outgoing_delivery(OutgoingUpdate::Failed, Delivery::Pending, true, 101, 0);
        assert_eq!(delivery, Delivery::Failed, "failed");
        snapshot.apply(AdapterEvent::MessageReplaced {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            old_id: "telegram:1:100".into(),
            message: outgoing(1, 101, "hi", delivery),
        });
        assert_eq!(snapshot.selected_messages().len(), 1);
        assert_eq!(snapshot.selected_messages()[0].id, "telegram:1:101");
        assert_eq!(snapshot.selected_messages()[0].delivery, Delivery::Failed);
        assert_ne!(snapshot.status_line(), SENDING_STATUS);

        snapshot.retry_send("telegram:1:101");
        let commands = snapshot.take_commands();
        assert_eq!(commands.len(), 1, "{commands:?}");
        assert!(matches!(
            &commands[0],
            AdapterCommand::ResendMessage {
                protocol: ProtocolId::Telegram,
                conversation_id,
                message_id,
                ..
            } if conversation_id == "telegram:1" && message_id == "telegram:1:101"
        ));
        assert_eq!(snapshot.selected_messages()[0].delivery, Delivery::Pending);
    }

    /// The payload `td_api.tl` describes. `updateMessageSendSucceeded.message`
    /// is the sent message: `sending_state` is null, which main maps to
    /// `Sent`. `updateMessageSendFailed.message` has a failed sending state,
    /// which main maps to `Failed`.
    #[test]
    fn a_null_sending_state_on_success_replaces_the_pending_row() {
        use thinwire_protocol::{OutgoingUpdate, outgoing_delivery};

        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        let _request = pending_telegram_send(&mut snapshot, 1, 100, "hi");
        assert_eq!(
            snapshot
                .selected_messages()
                .iter()
                .find(|row| row.body == "hi")
                .map(|row| row.delivery),
            Some(Delivery::Pending),
            "pending"
        );
        assert_eq!(
            snapshot
                .selected_messages()
                .iter()
                .filter(|row| row.body == "hi")
                .count(),
            1,
            "acknowledged"
        );

        let server = 1_048_576;
        // sending_state = null.
        let delivery =
            outgoing_delivery(OutgoingUpdate::Succeeded, Delivery::Sent, true, server, 0);
        assert_eq!(delivery, Delivery::Sent, "succeeded");
        snapshot.apply(AdapterEvent::MessageReplaced {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            old_id: "telegram:1:100".into(),
            message: outgoing(1, server, "hi", delivery),
        });
        // The chat's last message is the same server message, still null.
        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(1, server, "hi", Delivery::Sent),
        });
        let hi: Vec<_> = snapshot
            .selected_messages()
            .iter()
            .filter(|row| row.body == "hi")
            .collect();
        assert_eq!(hi.len(), 1, "succeeded");
        assert_eq!(hi[0].id, format!("telegram:1:{server}"));
        assert_eq!(hi[0].delivery, Delivery::Sent, "succeeded");
        assert_ne!(snapshot.status_line(), SENDING_STATUS, "succeeded");

        snapshot.apply(AdapterEvent::OutboxRead {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            last_read_message_id: server,
        });
        assert_eq!(
            snapshot
                .selected_messages()
                .iter()
                .find(|row| row.body == "hi")
                .map(|row| row.delivery),
            Some(Delivery::Read),
            "read outbox"
        );

        let mut failed = ready_with_chats(&store);
        let _request = pending_telegram_send(&mut failed, 1, 100, "nope");
        // sending_state = Failed.
        let failed_delivery =
            outgoing_delivery(OutgoingUpdate::Failed, Delivery::Failed, true, 101, 0);
        assert_eq!(failed_delivery, Delivery::Failed, "failed");
        failed.apply(AdapterEvent::MessageReplaced {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            old_id: "telegram:1:100".into(),
            message: outgoing(1, 101, "nope", failed_delivery),
        });
        assert_eq!(failed.selected_messages().len(), 1, "failed");
        assert_eq!(
            failed.selected_messages()[0].delivery,
            Delivery::Failed,
            "failed"
        );
        assert_ne!(failed.status_line(), SENDING_STATUS, "failed");
    }

    #[test]
    fn failed_send_keeps_the_text_and_retry_queues_one_resend() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        snapshot.compose = "hi".into();
        snapshot.send_compose();
        assert_eq!(send_texts(&mut snapshot), vec!["hi"]);
        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(1, 100, "hi", Delivery::Pending),
        });
        accept(&mut snapshot, "telegram:1");
        assert!(snapshot.compose.is_empty());
        snapshot.apply(AdapterEvent::MessageReplaced {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            old_id: "telegram:1:100".into(),
            message: outgoing(1, 101, "hi", Delivery::Failed),
        });
        assert_eq!(snapshot.selected_messages()[0].delivery, Delivery::Failed);
        assert_eq!(snapshot.compose, "hi", "failed text goes back to compose");
        let error = snapshot.error.clone().expect("error block");
        assert_eq!(error.happened, "Message not sent.");

        snapshot.retry_send("telegram:1:101");
        snapshot.retry_send("telegram:1:101");
        let commands = snapshot.take_commands();
        assert_eq!(commands.len(), 1, "{commands:?}");
        assert!(matches!(
            &commands[0],
            AdapterCommand::ResendMessage { protocol: ProtocolId::Telegram, conversation_id, message_id, .. }
                if conversation_id == "telegram:1" && message_id == "telegram:1:101"
        ));
        assert_eq!(snapshot.selected_messages()[0].delivery, Delivery::Pending);
        assert!(
            snapshot.compose.is_empty(),
            "retry does not leave a copy to send twice"
        );

        snapshot.apply(AdapterEvent::MessageDelivery {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            message_id: "telegram:1:101".into(),
            delivery: Delivery::Failed,
        });
        assert_eq!(snapshot.selected_messages()[0].delivery, Delivery::Failed);
        assert_eq!(snapshot.compose, "hi");
    }

    #[test]
    fn old_failed_rows_from_history_do_not_raise_an_error() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(1, 5, "old", Delivery::Failed),
        });
        assert!(snapshot.error.is_none());
        assert!(snapshot.compose.is_empty());
        snapshot.retry_send("telegram:1:5");
        assert_eq!(snapshot.take_commands().len(), 1);
    }

    #[test]
    fn failure_in_another_chat_goes_to_that_chat_draft() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(2, 100, "for Bob", Delivery::Pending),
        });
        snapshot.apply(AdapterEvent::MessageReplaced {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:2".into(),
            old_id: "telegram:2:100".into(),
            message: outgoing(2, 101, "for Bob", Delivery::Failed),
        });
        assert!(snapshot.compose.is_empty());
        snapshot.select_conversation("telegram:2".into());
        assert_eq!(snapshot.compose, "for Bob");
    }

    fn at_phone_step(store: &SecretStore) -> Snapshot {
        seed_override(store);
        let mut snapshot = Snapshot::new();
        snapshot.open_telegram(store);
        snapshot.apply(AdapterEvent::TelegramAuth {
            phase: TelegramAuthPhase::NeedPhone,
        });
        snapshot.take_commands();
        snapshot
    }

    #[test]
    fn auth_error_table_gives_each_refusal_its_own_copy() {
        let cases = [
            (
                Some(TelegramAuthError::PhoneInvalid),
                "Check the number. Use + and the country code.",
            ),
            (Some(TelegramAuthError::CodeInvalid), "Type it again."),
            (
                Some(TelegramAuthError::CodeExpired),
                "Press Send a new code.",
            ),
            (Some(TelegramAuthError::PasswordInvalid), "Type it again."),
            (
                Some(TelegramAuthError::FloodWait { seconds: 30 }),
                "Wait 1 minute, then try again.",
            ),
            (
                Some(TelegramAuthError::FloodWait { seconds: 125 }),
                "Wait 3 minutes, then try again.",
            ),
            (
                Some(TelegramAuthError::Other { code: 406 }),
                "Correct the field, or press Cancel.",
            ),
            (None, "Correct the field, or press Cancel."),
        ];
        for (reason, next) in cases {
            let error = auth_user_error(reason);
            assert_eq!(error.next, next, "{reason:?}");
            for text in [&error.happened, &error.why, &error.next] {
                assert!(!text.contains("adapter"), "{text}");
                assert!(!text.contains("TDLib"), "{text}");
            }
        }
        assert_eq!(
            auth_user_error(Some(TelegramAuthError::CodeInvalid)).why,
            "The code is wrong."
        );
        assert_eq!(
            auth_user_error(Some(TelegramAuthError::CodeExpired)).why,
            "The code expired."
        );
        assert_eq!(
            auth_user_error(Some(TelegramAuthError::PasswordInvalid)).why,
            "The password is wrong."
        );
        assert!(
            auth_user_error(Some(TelegramAuthError::Other { code: 406 }))
                .why
                .contains("406")
        );
    }

    #[test]
    fn rejected_step_shows_the_specific_error_then_clears() {
        let store = SecretStore::memory();
        let mut snapshot = at_phone_step(&store);
        snapshot.telegram_phone = "12".into();
        snapshot.auth_key(AuthKey::Enter, &store);
        snapshot.apply(AdapterEvent::TelegramAuthRejected {
            error: TelegramAuthError::PhoneInvalid,
        });
        snapshot.apply(AdapterEvent::TelegramAuth {
            phase: TelegramAuthPhase::Failed,
        });
        assert_eq!(snapshot.auth, AuthScreen::TelegramPhone);
        assert!(!snapshot.auth_busy);
        let error = snapshot.error.clone().expect("error");
        assert_eq!(error.next, "Check the number. Use + and the country code.");

        snapshot.telegram_phone = "+15551234567".into();
        snapshot.auth_key(AuthKey::Enter, &store);
        snapshot.apply(AdapterEvent::TelegramCodeSent {
            via: TelegramCodeVia::Sms,
        });
        snapshot.apply(AdapterEvent::TelegramAuth {
            phase: TelegramAuthPhase::NeedCode,
        });
        assert!(snapshot.error.is_none());
        assert_eq!(snapshot.auth_rejection, None);
        assert_eq!(snapshot.code_via, Some(TelegramCodeVia::Sms));

        snapshot.telegram_code = "11111".into();
        snapshot.auth_key(AuthKey::Enter, &store);
        snapshot.apply(AdapterEvent::TelegramAuth {
            phase: TelegramAuthPhase::Failed,
        });
        assert_eq!(
            snapshot.error.clone().expect("generic").why,
            "Telegram did not accept this step."
        );
    }

    #[test]
    fn enter_on_each_step_queues_exactly_one_auth_step() {
        let store = SecretStore::memory();
        let mut snapshot = at_phone_step(&store);
        snapshot.telegram_phone = "+15551234567".into();
        snapshot.auth_key(AuthKey::Enter, &store);
        snapshot.auth_key(AuthKey::Enter, &store);
        assert_eq!(auth_steps(&mut snapshot), vec![TelegramAuthStep::Phone]);
        snapshot.apply(AdapterEvent::TelegramAuth {
            phase: TelegramAuthPhase::NeedCode,
        });
        snapshot.telegram_code = "12345".into();
        snapshot.auth_key(AuthKey::Enter, &store);
        snapshot.auth_key(AuthKey::Enter, &store);
        assert_eq!(auth_steps(&mut snapshot), vec![TelegramAuthStep::Code]);
        snapshot.apply(AdapterEvent::TelegramAuth {
            phase: TelegramAuthPhase::NeedTwoFactor,
        });
        snapshot.telegram_2fa = "2fa-secret".into();
        snapshot.auth_key(AuthKey::Enter, &store);
        snapshot.auth_key(AuthKey::Enter, &store);
        assert_eq!(auth_steps(&mut snapshot), vec![TelegramAuthStep::TwoFactor]);
    }

    #[test]
    fn empty_two_step_password_cannot_submit() {
        let store = SecretStore::memory();
        let mut snapshot = at_phone_step(&store);
        snapshot.apply(AdapterEvent::TelegramAuth {
            phase: TelegramAuthPhase::NeedTwoFactor,
        });
        assert!(!snapshot.can_submit_auth());
        snapshot.auth_key(AuthKey::Enter, &store);
        snapshot.advance_telegram(&store);
        assert!(auth_steps(&mut snapshot).is_empty());
        assert!(!snapshot.auth_busy, "the screen does not hang in busy");
        snapshot.telegram_2fa = "x".into();
        assert!(snapshot.can_submit_auth());
    }

    #[test]
    fn escape_cancels_and_change_number_goes_back_without_a_command() {
        let store = SecretStore::memory();
        let mut snapshot = at_phone_step(&store);
        snapshot.telegram_phone = "+15551234567".into();
        snapshot.auth_key(AuthKey::Enter, &store);
        snapshot.apply(AdapterEvent::TelegramAuth {
            phase: TelegramAuthPhase::NeedCode,
        });
        snapshot.take_commands();
        snapshot.telegram_code = "123".into();
        snapshot.change_number();
        assert_eq!(snapshot.auth, AuthScreen::TelegramPhone);
        assert!(snapshot.telegram_code.is_empty());
        assert!(snapshot.take_commands().is_empty());

        snapshot.auth_key(AuthKey::Escape, &store);
        assert_eq!(snapshot.auth, AuthScreen::Idle);
        assert!(snapshot.telegram_phone.is_empty());
    }

    #[test]
    fn send_a_new_code_asks_telegram_to_resend_it() {
        let store = SecretStore::memory();
        let mut snapshot = at_phone_step(&store);
        snapshot.telegram_phone = "+15551234567".into();
        snapshot.auth_key(AuthKey::Enter, &store);
        snapshot.apply(AdapterEvent::TelegramAuth {
            phase: TelegramAuthPhase::NeedCode,
        });
        snapshot.take_commands();
        snapshot.apply(AdapterEvent::TelegramAuthRejected {
            error: TelegramAuthError::CodeExpired,
        });
        snapshot.apply(AdapterEvent::TelegramAuth {
            phase: TelegramAuthPhase::Failed,
        });
        snapshot.telegram_code = "11111".into();
        snapshot.resend_code();
        snapshot.resend_code();
        assert_eq!(
            auth_steps(&mut snapshot),
            vec![TelegramAuthStep::ResendCode],
            "resendAuthenticationCode, not a new phone submit"
        );
        assert_eq!(
            snapshot.auth,
            AuthScreen::TelegramCode,
            "the code step stays"
        );
        assert!(snapshot.telegram_code.is_empty());
        assert!(snapshot.auth_busy);
        snapshot.apply(AdapterEvent::TelegramCodeSent {
            via: TelegramCodeVia::Sms,
        });
        snapshot.apply(AdapterEvent::TelegramAuth {
            phase: TelegramAuthPhase::NeedCode,
        });
        assert!(!snapshot.auth_busy);
        assert_eq!(snapshot.code_via, Some(TelegramCodeVia::Sms));
    }

    #[test]
    fn change_number_submits_the_new_phone_from_the_code_step() {
        let store = SecretStore::memory();
        let mut snapshot = at_phone_step(&store);
        snapshot.telegram_phone = "+15551234567".into();
        snapshot.auth_key(AuthKey::Enter, &store);
        snapshot.apply(AdapterEvent::TelegramAuth {
            phase: TelegramAuthPhase::NeedCode,
        });
        snapshot.take_commands();
        snapshot.change_number();
        assert_eq!(snapshot.auth, AuthScreen::TelegramPhone);
        snapshot.telegram_phone = "+15557654321".into();
        snapshot.auth_key(AuthKey::Enter, &store);
        assert_eq!(
            auth_steps(&mut snapshot),
            vec![TelegramAuthStep::Phone],
            "setAuthenticationPhoneNumber in WaitCode moves TDLib to the new number"
        );
        assert_eq!(
            store.get(SecretKey::Phone).expect("phone").as_deref(),
            Some("+15557654321")
        );
    }

    #[test]
    fn enter_runs_the_main_button_on_each_center_screen() {
        let store = SecretStore::memory();
        seed_override(&store);
        let mut snapshot = Snapshot::new();
        settle_resume(&mut snapshot);
        assert_eq!(snapshot.center_view(), CenterView::FirstRun);
        snapshot.center_key(AuthKey::Enter, &store);
        assert_eq!(
            snapshot.auth,
            AuthScreen::TelegramConnecting,
            "Enter = Add Telegram"
        );
        assert_eq!(
            auth_steps(&mut snapshot),
            vec![TelegramAuthStep::ApiCredentials]
        );
        snapshot.apply(AdapterEvent::TelegramAuth {
            phase: TelegramAuthPhase::NeedPhone,
        });
        snapshot.telegram_phone = "+15551234567".into();
        snapshot.center_key(AuthKey::Enter, &store);
        assert_eq!(auth_steps(&mut snapshot), vec![TelegramAuthStep::Phone]);
        snapshot.center_key(AuthKey::Escape, &store);
        assert_eq!(snapshot.center_view(), CenterView::FirstRun);

        let missing = SecretStore::memory();
        let mut no_api = Snapshot::with_api_source(TelegramApiSource::empty());
        settle_resume(&mut no_api);
        no_api.center_key(AuthKey::Enter, &missing);
        assert_eq!(no_api.auth, AuthScreen::NeedCredentials);
        no_api.center_key(AuthKey::Enter, &missing);
        assert_eq!(no_api.auth, AuthScreen::TelegramApi, "Enter = Advanced");

        let mut ready = ready_with_chats(&store);
        ready.compose = "hi".into();
        ready.center_key(AuthKey::Enter, &store);
        assert!(
            ready.take_commands().is_empty(),
            "compose owns Enter in the thread"
        );
    }

    #[test]
    fn failed_client_setup_leaves_the_login_and_closes_the_client() {
        let store = SecretStore::memory();
        let mut snapshot = at_phone_step(&store);
        snapshot.apply(AdapterEvent::TelegramAuthRejected {
            error: TelegramAuthError::ClientSetup { code: 400 },
        });
        snapshot.apply(AdapterEvent::TelegramAuth {
            phase: TelegramAuthPhase::Failed,
        });
        assert_eq!(
            snapshot.auth,
            AuthScreen::Idle,
            "no phone step on a dead client"
        );
        assert!(!snapshot.auth_busy);
        let error = snapshot.error.clone().expect("error");
        assert_eq!(error.happened, "Telegram could not start.");
        assert!(error.why.contains("400"));
        assert!(snapshot.take_commands().iter().any(|command| matches!(
            command,
            AdapterCommand::Disconnect {
                protocol: ProtocolId::Telegram
            }
        )));
        assert_eq!(snapshot.center_view(), CenterView::FirstRun);
    }

    #[test]
    fn data_reset_shows_its_notice_on_the_next_phone_step() {
        let store = SecretStore::memory();
        seed_override(&store);
        store
            .set(SecretKey::Session, "tdlib-ready")
            .expect("marker");
        let mut snapshot = Snapshot::new();
        snapshot.try_resume(&store, true);
        snapshot.apply(AdapterEvent::TelegramDataReset {
            moved_to: "tdlib.stale-1790000000".into(),
        });
        snapshot.apply(AdapterEvent::TelegramAuth {
            phase: TelegramAuthPhase::NeedPhone,
        });
        assert_eq!(snapshot.auth, AuthScreen::TelegramPhone);
        let notice = snapshot.auth_notice.clone().expect("notice");
        assert!(notice.contains("\"tdlib.stale-1790000000\""), "{notice}");
        assert!(notice.contains("kept"), "never deleted (R62)");
        assert!(!notice.contains('/'), "a name, not a path");
        snapshot.telegram_phone = "+15551234567".into();
        submit_and_apply(&mut snapshot, &store, TelegramAuthPhase::NeedCode);
        assert_eq!(snapshot.auth_notice, None, "shown once");
        snapshot.apply(AdapterEvent::TelegramAuth {
            phase: TelegramAuthPhase::NeedPhone,
        });
        assert_eq!(snapshot.auth_notice, None, "the folder name shows once");
    }

    #[test]
    fn add_telegram_waits_for_the_keychain_read() {
        let store = SecretStore::detached_for_test();
        let mut snapshot =
            Snapshot::with_api_source(TelegramApiSource::with_publisher("11111", "publisher-hash"));
        snapshot.open_telegram(&store);
        assert_eq!(snapshot.auth, AuthScreen::Idle);
        assert!(
            snapshot.take_commands().is_empty(),
            "no client before the key is known"
        );
        store.complete_ready_attach_for_test(&[]);
        snapshot.open_telegram(&store);
        assert_eq!(snapshot.auth, AuthScreen::TelegramConnecting);
    }

    #[test]
    fn cancel_before_sign_in_still_closes_the_client() {
        let store = SecretStore::memory();
        let mut snapshot = at_phone_step(&store);
        assert!(snapshot.can_add_account());
        snapshot.cancel_auth(&store);
        assert!(snapshot.take_commands().iter().any(|command| matches!(
            command,
            AdapterCommand::Disconnect {
                protocol: ProtocolId::Telegram
            }
        )));
    }

    #[test]
    fn default_chrome_is_telegram_only_when_spikes_are_off() {
        assert!(protocol_chrome_enabled(ProtocolId::Telegram));
        assert_eq!(
            protocol_chrome_enabled(ProtocolId::Slack),
            cfg!(feature = "slack-oauth")
        );
        assert_eq!(
            protocol_chrome_enabled(ProtocolId::WhatsApp),
            cfg!(feature = "whatsapp-web")
        );
        assert_eq!(
            protocol_chrome_enabled(ProtocolId::Discord),
            cfg!(feature = "discord-bot")
        );
        assert_eq!(
            protocol_chrome_enabled(ProtocolId::Signal),
            cfg!(feature = "signal-local")
        );
        let snapshot = Snapshot::new();
        assert!(snapshot.shows_in_switcher(ProtocolId::Telegram));
        assert_eq!(
            snapshot.shows_in_switcher(ProtocolId::WhatsApp),
            cfg!(feature = "whatsapp-web")
        );
        assert_eq!(
            snapshot.shows_in_switcher(ProtocolId::Discord),
            DiscordAdapter::bot_inbox_compiled()
        );
        assert_eq!(
            snapshot.shows_in_switcher(ProtocolId::Slack),
            cfg!(feature = "slack-oauth")
        );
        assert_eq!(
            snapshot.shows_in_switcher(ProtocolId::Signal),
            cfg!(feature = "signal-local")
        );
    }

    #[cfg(feature = "signal-local")]
    #[test]
    fn linked_closes_the_signal_gate_without_cancelling_the_session() {
        let mut snapshot = Snapshot::new();
        snapshot.open_signal_notice();
        snapshot.acknowledge_signal_notice();
        snapshot.begin_signal_link();
        assert_eq!(snapshot.signal_screen, SignalScreen::Link);
        let _ = snapshot.take_commands();
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Signal,
            state: AccountState::Linked,
        });
        assert_eq!(snapshot.signal_screen, SignalScreen::Hidden);
        assert!(!snapshot.signal_gate_open());
        assert!(snapshot.signal_qr.is_none());
        assert_eq!(snapshot.selected_protocol, ProtocolId::Signal);
        assert!(
            snapshot
                .accounts
                .iter()
                .any(|row| row.caps.id == ProtocolId::Signal && row.linked())
        );
        assert!(
            !snapshot
                .take_commands()
                .contains(&AdapterCommand::SignalCancelLink)
        );
    }

    #[cfg(feature = "signal-local")]
    #[test]
    fn a_failed_link_can_start_again() {
        let mut snapshot = Snapshot::new();
        snapshot.open_signal_notice();
        snapshot.acknowledge_signal_notice();
        snapshot.begin_signal_link();
        assert!(snapshot.signal_started);
        let _ = snapshot.take_commands();
        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Signal,
            status: AdapterStatus::Error,
            detail: "Signal linking failed. No provisioning URL was logged.".into(),
        });
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Signal,
            state: AccountState::Unlinked,
        });
        assert!(!snapshot.signal_started);
        let error = snapshot.error.clone().expect("error");
        assert_eq!(error.happened, "Signal linking failed.");
        assert!(error.why.contains("No provisioning URL"));
        assert!(error.next.contains("Start linking again"));
    }

    #[cfg(feature = "signal-local")]
    #[test]
    fn a_sync_failure_after_link_keeps_the_error_and_allows_another_start() {
        let mut snapshot = Snapshot::new();
        snapshot.open_signal_notice();
        snapshot.acknowledge_signal_notice();
        snapshot.begin_signal_link();
        let _ = snapshot.take_commands();
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Signal,
            state: AccountState::Linked,
        });
        assert!(snapshot.signal_started);
        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Signal,
            status: AdapterStatus::Error,
            detail: "Signal chat list could not be read. Message text was not logged.".into(),
        });
        let error = snapshot.error.clone().expect("error");
        assert_eq!(error.happened, "Signal did not finish opening the inbox.");
        assert!(error.why.contains("chat list could not be read"));
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Signal,
            state: AccountState::Unlinked,
        });
        assert!(!snapshot.signal_started);
        assert!(snapshot.error.is_some());
    }

    #[cfg(feature = "signal-local")]
    #[test]
    fn an_operational_error_after_the_inbox_is_open_is_a_session_error() {
        let mut snapshot = signal_inbox_ready();
        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Signal,
            status: AdapterStatus::Error,
            detail: "Signal chat list could not be read. Message text was not logged.".into(),
        });
        let error = snapshot.error.clone().expect("error");
        assert_eq!(error.happened, "Signal: the last action did not finish.");
        assert!(error.why.contains("chat list could not be read"));
        assert_eq!(error.next, "Try again. The inbox stays open.");
        assert!(!error.next.contains("Start linking"));
        assert!(snapshot.signal_started);
        assert!(snapshot.signal_inbox_open);
        assert!(snapshot.protocol_linked(ProtocolId::Signal));
        assert_eq!(snapshot.signal_screen, SignalScreen::Hidden);
        snapshot.signal_screen = SignalScreen::Link;
        snapshot.begin_signal_link();
        assert!(
            !snapshot
                .take_commands()
                .iter()
                .any(|command| matches!(command, AdapterCommand::SignalBeginLink { .. })),
            "another link must not start while the worker is already linked"
        );
    }

    #[cfg(feature = "signal-local")]
    #[test]
    fn a_send_failure_after_the_inbox_is_open_keeps_the_send_error() {
        let mut snapshot = signal_inbox_ready();
        allow_send(&mut snapshot, ProtocolId::Signal);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Signal, "ada", true),
        });
        snapshot.compose = "hello".into();
        snapshot.send_compose();
        let request = snapshot
            .take_commands()
            .into_iter()
            .find_map(|command| match command {
                AdapterCommand::SendText {
                    protocol: ProtocolId::Signal,
                    request,
                    ..
                } => Some(request),
                _ => None,
            });
        let request = request.expect("send");
        snapshot.apply(AdapterEvent::SendRejected {
            protocol: ProtocolId::Signal,
            conversation_id: "ada".into(),
            request,
        });
        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Signal,
            status: AdapterStatus::Error,
            detail: "Signal send failed. The message text was not logged.".into(),
        });
        let error = snapshot.error.clone().expect("error");
        assert_eq!(error.happened, "Message not sent.");
        assert!(error.why.contains("did not accept"));
        assert!(!error.next.contains("Start linking"));
        assert!(error.next.contains("Send it again"));
        assert!(snapshot.signal_started);
        assert!(snapshot.protocol_linked(ProtocolId::Signal));
        assert_eq!(snapshot.signal_screen, SignalScreen::Hidden);
    }

    /// The phone removed this device. The account row stays. Local history
    /// stays. Sending is off. There is no error popup. Link opens the QR flow.
    #[cfg(feature = "signal-local")]
    #[test]
    fn a_removed_signal_device_keeps_history_and_opens_the_link_flow() {
        use thinwire_protocol::SIGNAL_DEVICE_REMOVED;

        let mut snapshot = signal_inbox_ready();
        assert!(snapshot.signal_notice_acknowledged);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Signal, "ada", true),
        });
        snapshot.apply(AdapterEvent::MessageReceived {
            message: ChatMessage {
                protocol: ProtocolId::Signal,
                conversation_id: "ada".into(),
                id: "ada:1".into(),
                sender: "Ada".into(),
                body: "kept".into(),
                outbound: false,
                delivery: Delivery::Sent,
                sent_at: 0,
                arrival: thinwire_protocol::Arrival::History,
                topic: None,
                links: Vec::new(),
                photo: None,
                audio: None,
                video: None,
                file: None,
                reactions: Vec::new(),
                reply_to: None,
                edited: false,
            },
        });
        snapshot.select_conversation("ada".into());
        snapshot.compose = "hello".into();
        let _ = snapshot.take_commands();
        assert_eq!(
            snapshot.status_text, "Signal is linked.",
            "a real link leaves this line"
        );

        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Signal,
            status: AdapterStatus::Error,
            detail: SIGNAL_DEVICE_REMOVED.into(),
        });
        snapshot.apply(AdapterEvent::AccountEnded {
            protocol: ProtocolId::Signal,
        });
        assert!(
            !snapshot
                .status_line()
                .to_ascii_lowercase()
                .contains("linked"),
            "after AccountEnded the strip must not say linked: {}",
            snapshot.status_line()
        );
        assert_ne!(
            snapshot.status_text, SIGNAL_DEVICE_REMOVED,
            "the strip does not repeat the row"
        );
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Signal,
            state: AccountState::Unlinked,
        });
        assert!(
            !snapshot
                .status_line()
                .to_ascii_lowercase()
                .contains("linked"),
            "Unlinked keeps the strip clear: {}",
            snapshot.status_line()
        );

        assert!(snapshot.signal_device_removed());
        assert!(
            snapshot.error.is_none(),
            "the line is not an error popup: {:?}",
            snapshot.error
        );
        let row = snapshot
            .accounts
            .iter()
            .find(|row| row.caps.id == ProtocolId::Signal)
            .expect("account row");
        assert_eq!(row.state, AccountState::Unlinked);
        assert_eq!(row.detail, SIGNAL_DEVICE_REMOVED);
        assert_eq!(snapshot.selected_conversation.as_deref(), Some("ada"));
        assert_eq!(snapshot.selected_messages().len(), 1);
        assert_eq!(snapshot.selected_messages()[0].body, "kept");
        assert!(
            snapshot
                .visible_conversations()
                .iter()
                .any(|row| row.id == "ada"),
            "the chat stays in the inbox"
        );
        assert!(!snapshot.can_send());
        snapshot.send_compose();
        assert!(
            snapshot
                .take_commands()
                .iter()
                .all(|command| !matches!(command, AdapterCommand::SendText { .. })),
            "sending is off"
        );
        assert_eq!(SIGNAL_SENDING_OFF, "Sending is off.");

        snapshot.relink_removed_signal();
        assert_eq!(snapshot.signal_screen, SignalScreen::Link);
        assert!(
            snapshot
                .take_commands()
                .iter()
                .any(|command| matches!(command, AdapterCommand::SignalBeginLink { .. })),
            "Link starts the QR flow"
        );
        assert_eq!(snapshot.selected_messages().len(), 1, "history stays");
    }

    /// The first unauthorized open shows the check. History stays. A later
    /// success, or any other error, drops that line and does not say removed.
    #[cfg(feature = "signal-local")]
    #[test]
    fn one_unauthorized_open_checks_the_row_and_keeps_history() {
        use thinwire_protocol::SIGNAL_LINK_CHECK;

        let mut snapshot = signal_inbox_ready();
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Signal, "ada", true),
        });
        snapshot.apply(AdapterEvent::MessageReceived {
            message: ChatMessage {
                protocol: ProtocolId::Signal,
                conversation_id: "ada".into(),
                id: "ada:1".into(),
                sender: "Ada".into(),
                body: "kept".into(),
                outbound: false,
                delivery: Delivery::Sent,
                sent_at: 0,
                arrival: thinwire_protocol::Arrival::History,
                topic: None,
                links: Vec::new(),
                photo: None,
                audio: None,
                video: None,
                file: None,
                reactions: Vec::new(),
                reply_to: None,
                edited: false,
            },
        });
        snapshot.select_conversation("ada".into());
        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Signal,
            status: AdapterStatus::Connecting,
            detail: SIGNAL_LINK_CHECK.into(),
        });
        assert!(snapshot.signal_link_checking());
        assert!(!snapshot.signal_device_removed());
        assert!(snapshot.error.is_none(), "{:?}", snapshot.error);
        let row = snapshot
            .accounts
            .iter()
            .find(|row| row.caps.id == ProtocolId::Signal)
            .expect("account row");
        assert_eq!(row.state, AccountState::Linked);
        assert_eq!(row.detail, SIGNAL_LINK_CHECK);
        assert_eq!(snapshot.selected_messages().len(), 1);
        assert_eq!(snapshot.selected_messages()[0].body, "kept");

        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Signal,
            state: AccountState::Linked,
        });
        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Signal,
            status: AdapterStatus::Ready,
            detail: "Signal is linked. Experimental client: expect breakage.".into(),
        });
        assert!(!snapshot.signal_link_checking());
        assert!(!snapshot.signal_device_removed());
        let row = snapshot
            .accounts
            .iter()
            .find(|row| row.caps.id == ProtocolId::Signal)
            .expect("account row");
        assert_eq!(row.status, AdapterStatus::Ready);
        assert_ne!(row.detail, SIGNAL_LINK_CHECK);
        assert_eq!(snapshot.selected_messages().len(), 1);

        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Signal,
            status: AdapterStatus::Connecting,
            detail: SIGNAL_LINK_CHECK.into(),
        });
        assert!(snapshot.signal_link_checking());
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Signal,
            state: AccountState::Linking,
        });
        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Signal,
            status: AdapterStatus::Connecting,
            detail: "Signal disconnected. The receive stream ended.".into(),
        });
        assert!(
            !snapshot.signal_link_checking(),
            "another error drops the check"
        );
        assert!(
            !snapshot.signal_device_removed(),
            "another error is not removal"
        );
        assert_eq!(snapshot.selected_messages().len(), 1, "history stays");
        let row = snapshot
            .accounts
            .iter()
            .find(|row| row.caps.id == ProtocolId::Signal)
            .expect("account row");
        assert_ne!(row.detail, thinwire_protocol::SIGNAL_DEVICE_REMOVED);
    }

    /// After removal, linking the same number again keeps history on the one
    /// Signal account row. Republishing the same chat does not add a second row.
    #[cfg(feature = "signal-local")]
    #[test]
    fn a_same_number_relink_keeps_history_on_the_existing_account() {
        use thinwire_protocol::SIGNAL_DEVICE_REMOVED;

        let mut snapshot = signal_inbox_ready();
        let conversation = chat(ProtocolId::Signal, "ada", true);
        let message = ChatMessage {
            protocol: ProtocolId::Signal,
            conversation_id: "ada".into(),
            id: "ada:1".into(),
            sender: "Ada".into(),
            body: "kept".into(),
            outbound: false,
            delivery: Delivery::Sent,
            sent_at: 0,
            arrival: thinwire_protocol::Arrival::History,
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
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: conversation.clone(),
        });
        snapshot.apply(AdapterEvent::MessageReceived {
            message: message.clone(),
        });
        snapshot.select_conversation("ada".into());

        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Signal,
            status: AdapterStatus::Error,
            detail: SIGNAL_DEVICE_REMOVED.into(),
        });
        snapshot.apply(AdapterEvent::AccountEnded {
            protocol: ProtocolId::Signal,
        });
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Signal,
            state: AccountState::Unlinked,
        });
        assert!(snapshot.signal_device_removed());
        assert_eq!(
            snapshot
                .accounts
                .iter()
                .filter(|row| row.caps.id == ProtocolId::Signal)
                .count(),
            1
        );

        snapshot.relink_removed_signal();
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Signal,
            state: AccountState::Linked,
        });
        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Signal,
            status: AdapterStatus::Ready,
            detail: "Signal is linked. Experimental client: expect breakage.".into(),
        });
        snapshot.apply(AdapterEvent::ConversationUpsert { conversation });
        snapshot.apply(AdapterEvent::MessageReceived { message });

        assert_eq!(
            snapshot
                .accounts
                .iter()
                .filter(|row| row.caps.id == ProtocolId::Signal)
                .count(),
            1,
            "the same number stays on the existing account row"
        );
        let row = snapshot
            .accounts
            .iter()
            .find(|row| row.caps.id == ProtocolId::Signal)
            .expect("account row");
        assert!(row.linked());
        assert!(!snapshot.signal_device_removed());
        assert_eq!(snapshot.visible_conversations().len(), 1);
        assert_eq!(snapshot.visible_conversations()[0].id, "ada");
        assert_eq!(snapshot.selected_conversation.as_deref(), Some("ada"));
        assert_eq!(snapshot.selected_messages().len(), 1);
        assert_eq!(snapshot.selected_messages()[0].id, "ada:1");
        assert_eq!(snapshot.selected_messages()[0].body, "kept");
        assert!(snapshot.signal_read_only_reason().is_none());
        assert!(
            snapshot
                .accounts
                .iter()
                .all(|row| row.signal_retained.is_none())
        );
    }

    /// A different number starts a new Signal row. The old row stays,
    /// read-only, with its history. It cannot send, and it says why.
    #[cfg(feature = "signal-local")]
    #[test]
    fn a_different_number_relink_keeps_the_old_row_read_only() {
        use thinwire_protocol::{
            SIGNAL_DEVICE_REMOVED, SIGNAL_PREVIOUS_ACCOUNT, signal_retained_chat_id,
        };

        let mut snapshot = signal_inbox_ready();
        let previous = chat(ProtocolId::Signal, "ada", true);
        let previous_message = ChatMessage {
            protocol: ProtocolId::Signal,
            conversation_id: "ada".into(),
            id: "ada:1".into(),
            sender: "Ada".into(),
            body: "kept".into(),
            outbound: false,
            delivery: Delivery::Sent,
            sent_at: 0,
            arrival: thinwire_protocol::Arrival::History,
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
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: previous,
        });
        snapshot.apply(AdapterEvent::MessageReceived {
            message: previous_message.clone(),
        });
        snapshot.select_conversation("ada".into());
        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Signal,
            status: AdapterStatus::Error,
            detail: SIGNAL_DEVICE_REMOVED.into(),
        });
        snapshot.apply(AdapterEvent::AccountEnded {
            protocol: ProtocolId::Signal,
        });
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Signal,
            state: AccountState::Unlinked,
        });
        snapshot.relink_removed_signal();

        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Signal,
            state: AccountState::Linked,
        });
        let kept_id = signal_retained_chat_id(1, "ada");
        let mut kept = chat(ProtocolId::Signal, &kept_id, false);
        kept.title = "Ada".into();
        let mut kept_message = previous_message;
        kept_message.conversation_id = kept_id.clone();
        kept_message.id = format!("{kept_id}:1");
        snapshot.apply(AdapterEvent::ConversationUpsert { conversation: kept });
        snapshot.apply(AdapterEvent::MessageReceived {
            message: kept_message,
        });
        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Signal,
            status: AdapterStatus::Ready,
            detail: SIGNAL_PREVIOUS_ACCOUNT.into(),
        });
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Signal, "bea", true),
        });
        snapshot.apply(AdapterEvent::MessageReceived {
            message: ChatMessage {
                protocol: ProtocolId::Signal,
                conversation_id: "bea".into(),
                id: "bea:1".into(),
                sender: "Bea".into(),
                body: "new number".into(),
                outbound: false,
                delivery: Delivery::Sent,
                sent_at: 0,
                arrival: thinwire_protocol::Arrival::History,
                topic: None,
                links: Vec::new(),
                photo: None,
                audio: None,
                video: None,
                file: None,
                reactions: Vec::new(),
                reply_to: None,
                edited: false,
            },
        });

        let signal_rows: Vec<_> = snapshot
            .accounts
            .iter()
            .filter(|row| row.caps.id == ProtocolId::Signal)
            .collect();
        assert_eq!(signal_rows.len(), 2, "a different number adds a row");
        assert!(signal_rows[0].signal_retained.is_none());
        assert!(signal_rows[0].linked(), "the new number is the live row");
        assert_eq!(signal_rows[1].signal_retained, Some(1));
        assert!(!signal_rows[1].linked());
        assert_eq!(signal_rows[1].detail, SIGNAL_PREVIOUS_ACCOUNT);
        assert_eq!(snapshot.visible_conversations().len(), 1);
        assert_eq!(snapshot.visible_conversations()[0].id, "bea");
        assert!(
            snapshot
                .visible_conversations()
                .iter()
                .all(|row| row.id != "ada"),
            "the old chat is not a second live row"
        );

        snapshot.select_conversation("bea".into());
        snapshot.compose = "hello".into();
        assert!(snapshot.can_send(), "the new number can send");

        snapshot.select_retained_signal(1);
        snapshot.compose = "hello".into();
        let retained_row = snapshot
            .accounts
            .iter()
            .find(|row| row.signal_retained == Some(1))
            .expect("retained row")
            .clone();
        let live_row = snapshot
            .accounts
            .iter()
            .find(|row| row.caps.id == ProtocolId::Signal && row.signal_retained.is_none())
            .expect("live row")
            .clone();
        assert!(snapshot.account_row_selected(&retained_row));
        assert!(!snapshot.account_row_selected(&live_row));
        assert_eq!(
            snapshot.signal_read_only_reason(),
            Some(SIGNAL_PREVIOUS_ACCOUNT)
        );
        assert_eq!(snapshot.visible_conversations().len(), 1);
        assert_eq!(snapshot.visible_conversations()[0].id, kept_id);
        assert!(!snapshot.visible_conversations()[0].writable);
        assert_eq!(snapshot.selected_messages().len(), 1);
        assert_eq!(snapshot.selected_messages()[0].body, "kept");
        assert!(!snapshot.can_send(), "the old row cannot send");
        snapshot.send_compose();
        assert!(
            snapshot
                .take_commands()
                .iter()
                .all(|command| !matches!(command, AdapterCommand::SendText { .. })),
            "sending from the old row does not go out"
        );
    }

    /// Two previous accounts get distinct chip text. The text is the slot.
    /// It has no phone number and no account id.
    #[test]
    fn two_previous_signal_accounts_have_distinct_labels() {
        use thinwire_protocol::signal_retained_chat_id;

        let mut snapshot = Snapshot::new();
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Signal,
            state: AccountState::Linked,
        });
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(
                ProtocolId::Signal,
                &signal_retained_chat_id(1, "ada"),
                false,
            ),
        });
        assert_eq!(
            snapshot.signal_retained_chip_label(1),
            thinwire_protocol::SIGNAL_PREVIOUS_ACCOUNT,
            "one previous account keeps the reason"
        );
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(
                ProtocolId::Signal,
                &signal_retained_chat_id(2, "bea"),
                false,
            ),
        });
        assert_eq!(snapshot.signal_retained_chip_label(1), "Previous account 1");
        assert_eq!(snapshot.signal_retained_chip_label(2), "Previous account 2");
        for label in [
            snapshot.signal_retained_chip_label(1),
            snapshot.signal_retained_chip_label(2),
        ] {
            assert!(
                !label.contains('-') && !label.contains('+'),
                "the label has no account id and no phone number: {label}"
            );
        }
    }

    /// Three previous accounts. The newest slot is first in the account list.
    /// The chip text stays "Previous account N" for slot N.
    #[test]
    fn the_newest_previous_signal_account_is_listed_first() {
        use thinwire_protocol::signal_retained_chat_id;

        let slots_of = |snapshot: &Snapshot| -> Vec<Option<u32>> {
            snapshot
                .accounts
                .iter()
                .filter(|row| row.caps.id == ProtocolId::Signal)
                .map(|row| row.signal_retained)
                .collect()
        };
        let mut snapshot = Snapshot::new();
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Signal,
            state: AccountState::Linked,
        });
        for slot in [1u32, 2, 3] {
            snapshot.apply(AdapterEvent::ConversationUpsert {
                conversation: chat(
                    ProtocolId::Signal,
                    &signal_retained_chat_id(slot, "ada"),
                    false,
                ),
            });
        }
        assert_eq!(
            slots_of(&snapshot),
            vec![None, Some(3), Some(2), Some(1)],
            "the highest slot is next to the live row"
        );
        assert_eq!(snapshot.signal_retained_chip_label(1), "Previous account 1");
        assert_eq!(snapshot.signal_retained_chip_label(2), "Previous account 2");
        assert_eq!(snapshot.signal_retained_chip_label(3), "Previous account 3");

        let mut snapshot = Snapshot::new();
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Signal,
            state: AccountState::Linked,
        });
        for slot in [1u32, 3, 2] {
            snapshot.apply(AdapterEvent::ConversationUpsert {
                conversation: chat(
                    ProtocolId::Signal,
                    &signal_retained_chat_id(slot, "ada"),
                    false,
                ),
            });
        }
        assert_eq!(slots_of(&snapshot), vec![None, Some(3), Some(2), Some(1)]);
        assert_eq!(snapshot.signal_retained_chip_label(1), "Previous account 1");
        assert_eq!(snapshot.signal_retained_chip_label(3), "Previous account 3");
    }

    /// A retained chat asks for the next page. Sending stays off.
    #[test]
    fn a_retained_signal_chat_requests_an_older_page() {
        use thinwire_protocol::signal_retained_chat_id;

        let mut snapshot = Snapshot::new();
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Signal,
            state: AccountState::Linked,
        });
        let id = signal_retained_chat_id(1, "ada");
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Signal, &id, false),
        });
        snapshot.apply(AdapterEvent::MessageReceived {
            message: ChatMessage {
                protocol: ProtocolId::Signal,
                conversation_id: id.clone(),
                id: format!("{id}:1"),
                sender: "Ada".into(),
                body: "kept".into(),
                outbound: false,
                delivery: Delivery::Sent,
                sent_at: 1,
                arrival: thinwire_protocol::Arrival::History,
                topic: None,
                links: Vec::new(),
                photo: None,
                audio: None,
                video: None,
                file: None,
                reactions: Vec::new(),
                reply_to: None,
                edited: false,
            },
        });
        snapshot.selected_protocol = ProtocolId::Signal;
        snapshot.selected_signal_retained = Some(1);
        snapshot.selected_conversation = Some(id.clone());
        snapshot.compose = "hello".into();
        assert!(!snapshot.can_send(), "a retained chat cannot send");
        snapshot.load_older_at(Instant::now());
        assert!(
            snapshot.take_commands().iter().any(|command| matches!(
                command,
                AdapterCommand::LoadOlderMessages { conversation_id, .. } if conversation_id == &id
            )),
            "the helper is asked for the older page"
        );
    }

    #[test]
    fn a_retained_signal_chat_loads_every_same_second_page() {
        use thinwire_protocol::signal_retained_chat_id;

        let base = 1_700_000_000_000u64;
        assert_eq!(
            base / 1_000,
            (base + 5) / 1_000,
            "every row shares one Unix second"
        );
        let mut snapshot = Snapshot::new();
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Signal,
            state: AccountState::Linked,
        });
        let id = signal_retained_chat_id(1, "ada");
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Signal, &id, false),
        });
        let row = |millis: u64, body: &str| ChatMessage {
            protocol: ProtocolId::Signal,
            conversation_id: id.clone(),
            id: format!("{millis}:ada"),
            sender: "Ada".into(),
            body: body.into(),
            outbound: false,
            delivery: Delivery::Sent,
            sent_at: i64::try_from(millis / 1_000).expect("seconds"),
            arrival: thinwire_protocol::Arrival::History,
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
        let pages = [
            [(base + 4, "e"), (base + 5, "f")],
            [(base + 2, "c"), (base + 3, "d")],
            [(base, "a"), (base + 1, "b")],
        ];
        for (millis, body) in pages[0] {
            snapshot.apply(AdapterEvent::MessageReceived {
                message: row(millis, body),
            });
        }
        snapshot.selected_protocol = ProtocolId::Signal;
        snapshot.selected_signal_retained = Some(1);
        snapshot.selected_conversation = Some(id.clone());
        snapshot.compose = "hello".into();
        assert!(!snapshot.can_send(), "a retained chat cannot send");
        let now = Instant::now();
        let asked = |snapshot: &mut Snapshot| {
            snapshot.load_older_at(now);
            let commands = snapshot.take_commands();
            assert!(
                commands.iter().all(|command| !matches!(
                    command,
                    AdapterCommand::SendText { .. }
                        | AdapterCommand::SendMedia { .. }
                        | AdapterCommand::SendTopicText { .. }
                        | AdapterCommand::ResendMessage { .. }
                )),
                "paging does not send: {commands:?}"
            );
            commands
                .into_iter()
                .filter_map(|command| match command {
                    AdapterCommand::LoadOlderMessages {
                        before_message_id, ..
                    } => Some(before_message_id),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(asked(&mut snapshot), vec![format!("{}:ada", base + 4)]);
        for (millis, body) in pages[1] {
            snapshot.apply(AdapterEvent::MessageReceived {
                message: row(millis, body),
            });
        }
        snapshot.apply(AdapterEvent::OlderHistoryLoaded {
            protocol: ProtocolId::Signal,
            conversation_id: id.clone(),
            before_message_id: format!("{}:ada", base + 4),
            more: true,
            note: None,
            topic: None,
            store_cursor: None,
        });
        assert_eq!(snapshot.older_state(), OlderState::Idle);
        assert_eq!(asked(&mut snapshot), vec![format!("{}:ada", base + 2)]);
        for (millis, body) in pages[2] {
            snapshot.apply(AdapterEvent::MessageReceived {
                message: row(millis, body),
            });
        }
        snapshot.apply(AdapterEvent::OlderHistoryLoaded {
            protocol: ProtocolId::Signal,
            conversation_id: id,
            before_message_id: format!("{}:ada", base + 2),
            more: false,
            note: None,
            topic: None,
            store_cursor: None,
        });
        let bodies: Vec<_> = snapshot
            .selected_messages()
            .iter()
            .map(|message| message.body.as_str())
            .collect();
        assert_eq!(bodies, ["a", "b", "c", "d", "e", "f"]);
        assert_eq!(snapshot.older_state(), OlderState::StartOfChat);
        assert!(
            asked(&mut snapshot).is_empty(),
            "the start of the chat asks for nothing"
        );
        assert!(!snapshot.can_send(), "a retained chat cannot send");
    }

    #[test]
    fn telegram_rows_in_one_second_keep_message_id_order() {
        let mut snapshot = Snapshot::new();
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Telegram,
            state: AccountState::Linked,
        });
        let chat_id = "telegram:1";
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Telegram, chat_id, true),
        });
        snapshot.selected_protocol = ProtocolId::Telegram;
        snapshot.selected_conversation = Some(chat_id.into());
        for (message_id, body) in [("telegram:1:50", "later"), ("telegram:1:40", "earlier")] {
            snapshot.apply(AdapterEvent::MessageReceived {
                message: ChatMessage {
                    protocol: ProtocolId::Telegram,
                    conversation_id: chat_id.into(),
                    id: message_id.into(),
                    sender: "Ada".into(),
                    body: body.into(),
                    outbound: false,
                    delivery: Delivery::Sent,
                    sent_at: 1_700_000_000,
                    arrival: thinwire_protocol::Arrival::History,
                    topic: None,
                    links: Vec::new(),
                    photo: None,
                    audio: None,
                    video: None,
                    file: None,
                    reactions: Vec::new(),
                    reply_to: None,
                    edited: false,
                },
            });
        }
        let ids: Vec<_> = snapshot
            .selected_messages()
            .iter()
            .map(|message| message.id.as_str())
            .collect();
        assert_eq!(ids, ["telegram:1:40", "telegram:1:50"]);
    }

    /// A slot that the next publish does not send is removed.
    /// The selected account does not stay on that slot.
    #[test]
    fn a_missing_retained_slot_is_dropped_on_the_next_publish() {
        use thinwire_protocol::{
            SIGNAL_RETAINED_RESET, SIGNAL_RETAINED_SETTLED, signal_retained_chat_id,
        };

        let mut snapshot = Snapshot::new();
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Signal,
            state: AccountState::Linked,
        });
        let old_id = signal_retained_chat_id(1, "ada");
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Signal, &old_id, false),
        });
        snapshot.apply(AdapterEvent::MessageReceived {
            message: ChatMessage {
                protocol: ProtocolId::Signal,
                conversation_id: old_id.clone(),
                id: format!("{old_id}:1"),
                sender: "Ada".into(),
                body: "old".into(),
                outbound: false,
                delivery: Delivery::Sent,
                sent_at: 1,
                arrival: thinwire_protocol::Arrival::History,
                topic: None,
                links: Vec::new(),
                photo: None,
                audio: None,
                video: None,
                file: None,
                reactions: Vec::new(),
                reply_to: None,
                edited: false,
            },
        });
        snapshot.selected_protocol = ProtocolId::Signal;
        snapshot.selected_signal_retained = Some(1);
        snapshot.selected_conversation = Some(old_id.clone());

        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Signal,
            status: AdapterStatus::Ready,
            detail: SIGNAL_RETAINED_RESET.into(),
        });
        assert!(
            snapshot
                .conversations
                .get(&ProtocolId::Signal)
                .is_some_and(|rows| rows.iter().any(|row| row.id == old_id)),
            "reset keeps the row until the publish settles"
        );
        assert_eq!(snapshot.selected_messages().len(), 1);
        let new_id = signal_retained_chat_id(2, "bea");
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Signal, &new_id, false),
        });
        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Signal,
            status: AdapterStatus::Ready,
            detail: SIGNAL_RETAINED_SETTLED.into(),
        });
        assert!(
            snapshot
                .accounts
                .iter()
                .any(|row| row.signal_retained == Some(2))
        );
        assert!(
            snapshot
                .accounts
                .iter()
                .all(|row| row.signal_retained != Some(1)),
            "slot 1 is not reused for the new account"
        );
        assert_eq!(snapshot.selected_signal_retained, None);
        assert_ne!(
            snapshot.selected_conversation.as_deref(),
            Some(old_id.as_str())
        );
    }

    /// A store that is still on disk and cannot be read keeps its row.
    /// A slot this publish does not send is still dropped.
    #[test]
    fn a_failed_retained_read_keeps_the_row_and_its_history() {
        use thinwire_protocol::{
            SIGNAL_PREVIOUS_ACCOUNT, SIGNAL_RETAINED_RESET, SIGNAL_RETAINED_SETTLED,
            SIGNAL_RETAINED_UNREADABLE, signal_retained_chat_id, signal_retained_unreadable_detail,
        };

        let mut snapshot = Snapshot::new();
        show_only(&mut snapshot, &[ProtocolId::Signal]);
        show_protocol(&mut snapshot, ProtocolId::Signal);
        let kept_id = signal_retained_chat_id(1, "ada");
        let missing_id = signal_retained_chat_id(2, "bea");
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Signal, &kept_id, false),
        });
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Signal, &missing_id, false),
        });
        snapshot.apply(AdapterEvent::MessageReceived {
            message: ChatMessage {
                protocol: ProtocolId::Signal,
                conversation_id: kept_id.clone(),
                id: format!("{kept_id}:1"),
                sender: "Ada".into(),
                body: "kept".into(),
                outbound: false,
                delivery: Delivery::Sent,
                sent_at: 1,
                arrival: thinwire_protocol::Arrival::History,
                topic: None,
                links: Vec::new(),
                photo: None,
                audio: None,
                video: None,
                file: None,
                reactions: Vec::new(),
                reply_to: None,
                edited: false,
            },
        });
        snapshot.select_retained_signal(1);

        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Signal,
            status: AdapterStatus::Ready,
            detail: SIGNAL_RETAINED_RESET.into(),
        });
        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Signal,
            status: AdapterStatus::Error,
            detail: signal_retained_unreadable_detail(1),
        });
        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Signal,
            status: AdapterStatus::Ready,
            detail: SIGNAL_RETAINED_SETTLED.into(),
        });

        let kept = snapshot
            .accounts
            .iter()
            .find(|row| row.signal_retained == Some(1))
            .expect("the row stays in the list");
        assert_eq!(kept.status, AdapterStatus::Error);
        assert_eq!(kept.retained_read, RetainedRead::Unreadable);
        assert_eq!(kept.detail, SIGNAL_RETAINED_UNREADABLE);
        assert!(
            !kept.detail.chars().any(|ch| ch.is_ascii_digit()),
            "the user-facing text has no slot count"
        );
        assert!(kept.history_unreadable(), "the row is disabled");
        assert_eq!(snapshot.signal_retained_chip_label(1), kept.detail);
        snapshot.compose = "hello".into();
        assert!(!snapshot.can_send(), "the row has no Send");
        assert!(snapshot.hides_composer(), "the row has no composer");
        assert!(
            snapshot
                .accounts
                .iter()
                .all(|row| row.signal_retained != Some(2)),
            "a slot this publish did not send is dropped"
        );
        assert!(
            snapshot
                .conversations
                .get(&ProtocolId::Signal)
                .is_some_and(|rows| rows.iter().all(|row| row.id != missing_id)),
            "the missing slot's chats go with the row"
        );
        assert_eq!(snapshot.selected_signal_retained, Some(1));
        assert_eq!(snapshot.selected_messages().len(), 1);
        assert_eq!(snapshot.selected_messages()[0].body, "kept");
        let live = snapshot
            .accounts
            .iter()
            .find(|row| row.caps.id == ProtocolId::Signal && row.signal_retained.is_none())
            .expect("live row");
        assert_ne!(live.detail, SIGNAL_RETAINED_UNREADABLE);

        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Signal, &kept_id, false),
        });
        let restored = snapshot
            .accounts
            .iter()
            .find(|row| row.signal_retained == Some(1))
            .expect("row");
        assert_eq!(restored.detail, SIGNAL_PREVIOUS_ACCOUNT);
        assert_eq!(restored.retained_read, RetainedRead::Readable);
        assert!(!restored.history_unreadable());
        assert!(!snapshot.hides_composer());
        assert!(!snapshot.can_send(), "a previous account still cannot send");
        assert_eq!(snapshot.selected_messages()[0].body, "kept");
    }

    /// The disabled flag is the read state. The row text is not that state.
    #[test]
    fn an_unreadable_flag_ignores_the_detail_text() {
        use thinwire_protocol::{SIGNAL_RETAINED_UNREADABLE, signal_retained_unreadable_detail};

        let mut snapshot = Snapshot::new();
        show_only(&mut snapshot, &[ProtocolId::Signal]);
        show_protocol(&mut snapshot, ProtocolId::Signal);
        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Signal,
            status: AdapterStatus::Error,
            detail: signal_retained_unreadable_detail(1),
        });
        let row = snapshot
            .accounts
            .iter_mut()
            .find(|row| row.signal_retained == Some(1))
            .expect("row");
        assert_eq!(row.retained_read, RetainedRead::Unreadable);
        row.detail = "other line".into();
        assert!(
            row.history_unreadable(),
            "a different detail leaves the row disabled"
        );
        row.retained_read = RetainedRead::Readable;
        row.detail = SIGNAL_RETAINED_UNREADABLE.to_owned();
        assert!(
            !row.history_unreadable(),
            "the sentence alone does not disable the row"
        );
    }

    /// A helper that leaves, and an account that ends, close an open
    /// retained publish. A later settled line does not drop the row.
    #[test]
    fn a_helper_stop_or_account_end_closes_an_open_retained_publish() {
        use thinwire_protocol::{
            SIGNAL_RETAINED_RESET, SIGNAL_RETAINED_SETTLED, signal_retained_chat_id,
        };

        fn open_publish() -> (Snapshot, String) {
            let mut snapshot = Snapshot::new();
            let kept_id = signal_retained_chat_id(1, "ada");
            snapshot.apply(AdapterEvent::ConversationUpsert {
                conversation: chat(ProtocolId::Signal, &kept_id, false),
            });
            snapshot.apply(AdapterEvent::MessageReceived {
                message: ChatMessage {
                    protocol: ProtocolId::Signal,
                    conversation_id: kept_id.clone(),
                    id: format!("{kept_id}:1"),
                    sender: "Ada".into(),
                    body: "kept".into(),
                    outbound: false,
                    delivery: Delivery::Sent,
                    sent_at: 1,
                    arrival: thinwire_protocol::Arrival::History,
                    topic: None,
                    links: Vec::new(),
                    photo: None,
                    audio: None,
                    video: None,
                    file: None,
                    reactions: Vec::new(),
                    reply_to: None,
                    edited: false,
                },
            });
            snapshot.apply(AdapterEvent::Status {
                protocol: ProtocolId::Signal,
                status: AdapterStatus::Ready,
                detail: SIGNAL_RETAINED_RESET.into(),
            });
            assert!(snapshot.retained_publish.is_some());
            (snapshot, kept_id)
        }

        fn settled_keeps_the_row(snapshot: &mut Snapshot, kept_id: &str) {
            snapshot.apply(AdapterEvent::Status {
                protocol: ProtocolId::Signal,
                status: AdapterStatus::Ready,
                detail: SIGNAL_RETAINED_SETTLED.into(),
            });
            assert!(snapshot.retained_publish.is_none());
            assert!(
                snapshot
                    .accounts
                    .iter()
                    .any(|row| row.signal_retained == Some(1)),
                "the row stays after the publish is closed"
            );
            assert!(
                snapshot
                    .messages
                    .values()
                    .flatten()
                    .any(|message| message.conversation_id == kept_id && message.body == "kept"),
                "the history stays"
            );
        }

        for state in [
            HelperState::Idle,
            HelperState::Restarting,
            HelperState::Stopped(HelperFault::Crashed),
            HelperState::Missing,
        ] {
            let (mut snapshot, kept_id) = open_publish();
            snapshot.apply(AdapterEvent::Helper {
                protocol: ProtocolId::Signal,
                state,
            });
            assert!(
                snapshot.retained_publish.is_none(),
                "the helper left, so the open publish is closed: {state:?}"
            );
            settled_keeps_the_row(&mut snapshot, &kept_id);
        }

        let (mut ended, kept_id) = open_publish();
        ended.apply(AdapterEvent::AccountEnded {
            protocol: ProtocolId::Signal,
        });
        assert!(ended.retained_publish.is_none());
        settled_keeps_the_row(&mut ended, &kept_id);

        let (mut unlinked, kept_id) = open_publish();
        unlinked.apply(AdapterEvent::Account {
            protocol: ProtocolId::Signal,
            state: AccountState::Linked,
        });
        unlinked.apply(AdapterEvent::Status {
            protocol: ProtocolId::Signal,
            status: AdapterStatus::Ready,
            detail: SIGNAL_RETAINED_RESET.into(),
        });
        unlinked.apply(AdapterEvent::Account {
            protocol: ProtocolId::Signal,
            state: AccountState::Unlinked,
        });
        assert!(unlinked.retained_publish.is_none());
        settled_keeps_the_row(&mut unlinked, &kept_id);
    }

    /// After the session ends, the retained rows and their history stay.
    /// Compose stays off and says why. The live row stays signed out.
    /// The old "shows again after you link" line is gone.
    #[test]
    fn a_session_end_keeps_retained_rows_and_their_history() {
        use thinwire_protocol::{SIGNAL_PREVIOUS_ACCOUNT, signal_retained_chat_id};

        let mut snapshot = Snapshot::new();
        show_only(&mut snapshot, &[ProtocolId::Signal]);
        show_protocol(&mut snapshot, ProtocolId::Signal);
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Signal,
            state: AccountState::Linked,
        });
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Signal, "live-ada", true),
        });
        let kept_id = signal_retained_chat_id(1, "ada");
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Signal, &kept_id, false),
        });
        snapshot.apply(AdapterEvent::MessageReceived {
            message: ChatMessage {
                protocol: ProtocolId::Signal,
                conversation_id: kept_id.clone(),
                id: format!("{kept_id}:1"),
                sender: "Ada".into(),
                body: "kept".into(),
                outbound: false,
                delivery: Delivery::Sent,
                sent_at: 1,
                arrival: thinwire_protocol::Arrival::History,
                topic: None,
                links: Vec::new(),
                photo: None,
                audio: None,
                video: None,
                file: None,
                reactions: Vec::new(),
                reply_to: None,
                edited: false,
            },
        });
        snapshot.select_retained_signal(1);
        snapshot.compose = "hello".into();
        assert!(!snapshot.can_send());
        assert_eq!(
            snapshot.signal_read_only_reason(),
            Some(SIGNAL_PREVIOUS_ACCOUNT)
        );

        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Signal,
            state: AccountState::Unlinked,
        });

        assert!(!snapshot.has_session(ProtocolId::Signal));
        let live = snapshot
            .accounts
            .iter()
            .find(|row| row.caps.id == ProtocolId::Signal && row.signal_retained.is_none())
            .expect("live row");
        assert!(!live.linked(), "the live row is signed out");
        assert_ne!(live.detail, SIGNAL_PREVIOUS_ACCOUNT);
        let retained = snapshot
            .accounts
            .iter()
            .find(|row| row.signal_retained == Some(1))
            .expect("retained row");
        assert_eq!(retained.detail, SIGNAL_PREVIOUS_ACCOUNT);
        assert_eq!(
            snapshot.signal_retained_chip_label(1),
            SIGNAL_PREVIOUS_ACCOUNT
        );
        assert_eq!(snapshot.center_view(), CenterView::Thread);
        assert_eq!(snapshot.visible_conversations().len(), 1);
        assert_eq!(snapshot.visible_conversations()[0].id, kept_id);
        assert!(!snapshot.visible_conversations()[0].writable);
        assert_eq!(snapshot.selected_messages().len(), 1);
        assert_eq!(snapshot.selected_messages()[0].body, "kept");
        assert!(
            snapshot
                .conversations
                .get(&ProtocolId::Signal)
                .is_some_and(|rows| rows.iter().all(|row| row.id != "live-ada")),
            "the live chat ends with the session"
        );
        assert!(!snapshot.can_send(), "compose stays off");
        assert_eq!(
            snapshot.signal_read_only_reason(),
            Some(SIGNAL_PREVIOUS_ACCOUNT)
        );
        snapshot.send_compose();
        assert!(
            snapshot
                .take_commands()
                .iter()
                .all(|command| !matches!(command, AdapterCommand::SendText { .. })),
            "sending does not go out"
        );
        snapshot.load_older_at(Instant::now());
        assert!(
            snapshot.take_commands().iter().any(|command| matches!(
                command,
                AdapterCommand::LoadOlderMessages { conversation_id, .. }
                    if conversation_id == &kept_id
            )),
            "history still pages from retained/"
        );
        assert!(
            !snapshot.status_text.contains("shows again after you link")
                && snapshot
                    .accounts
                    .iter()
                    .all(|row| !row.detail.contains("shows again after you link")),
            "the held line is gone"
        );
    }

    /// A rejected Signal send stays on the previous-account row after the
    /// session ends. The live chat is gone. Sending stays off.
    #[test]
    fn a_rejected_signal_send_stays_on_the_retained_row() {
        use thinwire_protocol::signal_retained_chat_id;

        let mut snapshot = shell_with(&[ProtocolId::Signal]);
        link(&mut snapshot, ProtocolId::Signal);
        allow_send(&mut snapshot, ProtocolId::Signal);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Signal, "ada", true),
        });
        let kept_id = signal_retained_chat_id(1, "ada");
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Signal, &kept_id, false),
        });
        snapshot.select_conversation("ada".into());
        snapshot.compose = "unsent".into();
        snapshot.send_compose();
        let request = snapshot
            .take_commands()
            .into_iter()
            .find_map(|command| match command {
                AdapterCommand::SendText { request, .. } => Some(request),
                _ => None,
            })
            .expect("send");
        snapshot.apply(AdapterEvent::SendRejected {
            protocol: ProtocolId::Signal,
            conversation_id: "ada".into(),
            request,
        });
        assert_eq!(snapshot.compose, "unsent");

        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Signal,
            state: AccountState::Unlinked,
        });

        let draft = |(id, text): (&str, &str)| {
            snapshot
                .drafts
                .get(&(ProtocolId::Signal, id.to_owned(), None))
                .map(String::as_str)
                == Some(text)
        };
        assert!(draft(("ada", "unsent")), "a later link still has the text");
        assert!(
            draft((&kept_id, "unsent")),
            "the previous-account row keeps the text"
        );
        snapshot.select_retained_signal(1);
        assert_eq!(
            snapshot.selected_conversation.as_deref(),
            Some(kept_id.as_str())
        );
        assert_eq!(snapshot.compose, "unsent");
        assert!(!snapshot.can_send(), "the retained row cannot send");
        assert!(
            snapshot
                .conversations
                .get(&ProtocolId::Signal)
                .is_some_and(|rows| rows.iter().all(|row| row.id != "ada")),
            "the live chat ends with the session"
        );
    }

    /// A startup publish has no live session. Reset does not hide the rows.
    /// Each new slot appears when its store sends it. A slot the publish
    /// does not send leaves only when the publish settles.
    #[test]
    fn a_startup_publish_keeps_retained_rows_across_reset() {
        use thinwire_protocol::{
            SIGNAL_RETAINED_RESET, SIGNAL_RETAINED_SETTLED, signal_retained_chat_id,
        };

        let kept_line = "shows again after you link";
        let mut snapshot = Snapshot::new();
        show_only(&mut snapshot, &[ProtocolId::Signal]);
        show_protocol(&mut snapshot, ProtocolId::Signal);
        assert!(!snapshot.has_session(ProtocolId::Signal));
        let first = signal_retained_chat_id(1, "ada");
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Signal, &first, false),
        });
        snapshot.apply(AdapterEvent::MessageReceived {
            message: ChatMessage {
                protocol: ProtocolId::Signal,
                conversation_id: first.clone(),
                id: format!("{first}:1"),
                sender: "Ada".into(),
                body: "kept".into(),
                outbound: false,
                delivery: Delivery::Sent,
                sent_at: 1,
                arrival: thinwire_protocol::Arrival::History,
                topic: None,
                links: Vec::new(),
                photo: None,
                audio: None,
                video: None,
                file: None,
                reactions: Vec::new(),
                reply_to: None,
                edited: false,
            },
        });
        snapshot.select_retained_signal(1);
        assert_eq!(snapshot.visible_conversations().len(), 1);
        assert_eq!(snapshot.selected_messages()[0].body, "kept");

        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Signal,
            status: AdapterStatus::Ready,
            detail: SIGNAL_RETAINED_RESET.into(),
        });
        assert!(
            snapshot
                .accounts
                .iter()
                .any(|row| row.signal_retained == Some(1)),
            "reset does not hide the open row"
        );
        assert_eq!(snapshot.selected_messages().len(), 1);
        assert_eq!(snapshot.thread_state(), ThreadState::Rows);

        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Signal, &first, false),
        });
        let second = signal_retained_chat_id(2, "bea");
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Signal, &second, false),
        });
        assert!(
            snapshot
                .accounts
                .iter()
                .any(|row| row.signal_retained == Some(1))
                && snapshot
                    .accounts
                    .iter()
                    .any(|row| row.signal_retained == Some(2)),
            "each store shows its row when that store is open"
        );
        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Signal,
            status: AdapterStatus::Ready,
            detail: SIGNAL_RETAINED_SETTLED.into(),
        });
        assert!(
            snapshot
                .accounts
                .iter()
                .any(|row| row.signal_retained == Some(1))
        );
        assert_eq!(snapshot.selected_messages()[0].body, "kept");
        assert!(
            !snapshot.status_text.contains(kept_line)
                && snapshot
                    .accounts
                    .iter()
                    .all(|row| !row.detail.contains(kept_line)),
            "the held line never appears"
        );
        let live = snapshot
            .accounts
            .iter()
            .find(|row| row.caps.id == ProtocolId::Signal && row.signal_retained.is_none())
            .expect("live row");
        assert!(!live.linked());
    }

    #[cfg(feature = "signal-local")]
    fn signal_inbox_ready() -> Snapshot {
        let mut snapshot = Snapshot::new();
        snapshot.open_signal_notice();
        snapshot.acknowledge_signal_notice();
        snapshot.begin_signal_link();
        let _ = snapshot.take_commands();
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Signal,
            state: AccountState::Linked,
        });
        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Signal,
            status: AdapterStatus::Ready,
            detail: "Signal is linked. Experimental client: expect breakage.".into(),
        });
        assert!(snapshot.signal_inbox_open);
        snapshot
    }

    #[cfg(feature = "signal-local")]
    #[test]
    fn acknowledging_the_notice_leaves_a_linked_account_alone() {
        let mut snapshot = Snapshot::new();
        snapshot.open_signal_notice();
        snapshot.acknowledge_signal_notice();
        snapshot.begin_signal_link();
        let _ = snapshot.take_commands();
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Signal,
            state: AccountState::Linked,
        });
        snapshot.open_signal_notice();
        assert_eq!(snapshot.signal_screen, SignalScreen::Notice);
        snapshot.acknowledge_signal_notice();
        assert_eq!(snapshot.signal_screen, SignalScreen::Hidden);
        assert!(
            !snapshot
                .take_commands()
                .contains(&AdapterCommand::SignalCancelLink)
        );
        assert!(
            snapshot
                .accounts
                .iter()
                .any(|row| row.caps.id == ProtocolId::Signal && row.linked())
        );
    }

    #[cfg(feature = "signal-local")]
    #[test]
    fn reviewing_the_notice_keeps_a_linked_session() {
        let mut snapshot = Snapshot::new();
        snapshot.open_signal_notice();
        snapshot.acknowledge_signal_notice();
        snapshot.begin_signal_link();
        let _ = snapshot.take_commands();
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Signal,
            state: AccountState::Linked,
        });
        snapshot.open_signal_notice();
        assert_eq!(snapshot.signal_screen, SignalScreen::Notice);
        assert!(
            !snapshot
                .take_commands()
                .contains(&AdapterCommand::SignalCancelLink)
        );
        snapshot.close_signal_gate();
        assert_eq!(snapshot.signal_screen, SignalScreen::Hidden);
        assert!(
            !snapshot
                .take_commands()
                .contains(&AdapterCommand::SignalCancelLink)
        );
        assert!(
            snapshot
                .accounts
                .iter()
                .any(|row| row.caps.id == ProtocolId::Signal && row.linked())
        );
    }

    #[cfg(feature = "signal-local")]
    #[test]
    fn an_older_signal_qr_is_dropped() {
        use thinwire_protocol::RedactedPairingSecret;
        let mut snapshot = Snapshot::new();
        snapshot.open_signal_notice();
        snapshot.acknowledge_signal_notice();
        snapshot.begin_signal_link();
        let AdapterCommand::SignalBeginLink { generation } = snapshot
            .take_commands()
            .into_iter()
            .find(|command| matches!(command, AdapterCommand::SignalBeginLink { .. }))
            .expect("begin")
        else {
            panic!("begin link");
        };
        snapshot.apply(AdapterEvent::SignalQr {
            code: RedactedPairingSecret::new("sgnl://old"),
            generation: generation.saturating_sub(1),
        });
        assert!(snapshot.signal_qr.is_none());
        snapshot.apply(AdapterEvent::SignalQr {
            code: RedactedPairingSecret::new("sgnl://current"),
            generation,
        });
        assert_eq!(snapshot.signal_qr.as_deref(), Some("sgnl://current"));
    }

    #[test]
    fn first_run_without_credentials_does_not_open_api_screens() {
        let store = SecretStore::memory();
        // A local shell can inject TELEGRAM_API_ID at build time; this case has none.
        let mut snapshot = Snapshot::with_api_source(TelegramApiSource::empty());
        snapshot.open_add_account(&store);
        assert_eq!(snapshot.auth, AuthScreen::NeedCredentials);
        assert!(snapshot.status_text.contains("Credentials missing"));
        assert!(!snapshot.status_text.contains("my.telegram.org"));
        snapshot.advance_telegram(&store);
        assert_eq!(snapshot.auth, AuthScreen::NeedCredentials);
        assert!(store.get(SecretKey::ApiId).expect("get").is_none());
    }

    #[test]
    fn council_adrs_use_locked_filenames() {
        let six = include_str!("../../../decisions/0006-live-tdlib.md");
        assert!(six.contains("# Live TDLib replaces the Telegram stub"));
        assert!(six.contains("authorizationStateReady"));
        assert!(six.contains("0007-publisher-telegram-api-credentials"));
        let seven = include_str!("../../../decisions/0007-publisher-telegram-api-credentials.md");
        assert!(seven.contains("# Publisher-owned Telegram api_id / api_hash"));
        assert!(seven.contains("Primary login UI: phone/code"));
        assert!(seven.contains("not** the primary login path"));
        assert!(seven.contains("do **not** send every user to my.telegram.org"));
        assert!(seven.contains("GitHub Actions repository secrets"));
        assert!(seven.contains("TELEGRAM_API_ID"));
        assert!(seven.contains("win."));
        assert!(seven.contains("Encrypted inventory copy"));
        assert!(seven.contains("Terraform/SOPS"));
        let roadmap = include_str!("../../../ROADMAP.md");
        assert!(roadmap.contains("#19"));
        assert!(roadmap.contains("5c46222"));
        assert!(roadmap.contains("authorizationStateReady"));
        assert!(roadmap.contains("Chat list + messages"));
    }

    #[test]
    fn publisher_inject_skips_api_screens() {
        let store = SecretStore::memory();
        let mut snapshot =
            Snapshot::with_api_source(TelegramApiSource::with_publisher("11111", "publisher-hash"));
        snapshot.open_telegram(&store);
        assert_eq!(snapshot.auth, AuthScreen::TelegramConnecting);
        assert!(snapshot.auth_busy);
        assert!(!snapshot.telegram_ready());
        let commands = snapshot.take_commands();
        assert!(commands.iter().any(|c| matches!(
            c,
            AdapterCommand::TelegramAuth {
                step: TelegramAuthStep::ApiCredentials,
                epoch: 0
            }
        )));
        assert!(!format!("{commands:?}").contains("publisher-hash"));
    }

    #[test]
    fn keychain_override_skips_api_screens() {
        let store = SecretStore::memory();
        seed_override(&store);
        let mut snapshot = Snapshot::new();
        snapshot.open_telegram(&store);
        assert_eq!(snapshot.auth, AuthScreen::TelegramConnecting);
        assert!(snapshot.has_api_credentials(&store));
    }

    #[test]
    fn telegram_auth_waits_for_adapter_phase_events() {
        let store = SecretStore::memory();
        seed_override(&store);
        let mut snapshot = Snapshot::new();
        snapshot.open_telegram(&store);
        assert_eq!(snapshot.auth, AuthScreen::TelegramConnecting);
        assert!(snapshot.auth_busy);
        snapshot.apply(AdapterEvent::TelegramAuth {
            phase: TelegramAuthPhase::NeedPhone,
        });
        assert_eq!(snapshot.auth, AuthScreen::TelegramPhone);
        assert!(!snapshot.auth_busy);
        snapshot.telegram_phone = "+15551234567".into();
        snapshot.advance_telegram(&store);
        snapshot.apply(AdapterEvent::TelegramAuth {
            phase: TelegramAuthPhase::NeedCode,
        });
        assert_eq!(snapshot.auth, AuthScreen::TelegramCode);
        snapshot.telegram_code = "12345".into();
        snapshot.advance_telegram(&store);
        snapshot.apply(AdapterEvent::TelegramAuth {
            phase: TelegramAuthPhase::NeedTwoFactor,
        });
        assert_eq!(snapshot.auth, AuthScreen::Telegram2fa);
        assert!(!snapshot.telegram_ready());
    }

    #[test]
    fn busy_submit_does_not_queue_a_second_command() {
        let store = SecretStore::memory();
        seed_override(&store);
        let mut snapshot = Snapshot::new();
        snapshot.open_telegram(&store);
        assert_eq!(snapshot.take_commands().len(), 1);
        snapshot.advance_telegram(&store);
        assert!(snapshot.take_commands().is_empty());
        assert!(snapshot.auth_busy);
    }

    #[test]
    fn empty_override_fields_do_not_advance() {
        let store = SecretStore::memory();
        let mut snapshot = Snapshot::new();
        snapshot.open_api_override(&store);
        snapshot.advance_telegram(&store);
        assert_eq!(snapshot.auth, AuthScreen::TelegramApi);
        assert!(snapshot.error.is_some());
        assert!(store.get(SecretKey::ApiId).expect("get").is_none());
    }

    #[test]
    fn cancel_always_returns_to_idle_and_clears_fields() {
        let store = SecretStore::memory();
        seed_override(&store);
        let mut snapshot = Snapshot::new();
        snapshot.open_telegram(&store);
        snapshot.apply(AdapterEvent::TelegramAuth {
            phase: TelegramAuthPhase::NeedPhone,
        });
        snapshot.telegram_phone = "+15551234567".into();
        snapshot.advance_telegram(&store);
        snapshot.cancel_auth(&store);
        assert_eq!(snapshot.auth, AuthScreen::Idle);
        assert!(snapshot.telegram_phone.is_empty());
        assert!(!snapshot.telegram_ready());
    }

    #[test]
    fn telegram_flow_stores_secrets_and_lands_in_inbox() {
        let store = SecretStore::memory();
        let mut snapshot = Snapshot::new();
        let saved = AdapterEvent::ConversationUpsert {
            conversation: Conversation {
                protocol: ProtocolId::Telegram,
                id: "telegram:saved".into(),
                title: "Saved Messages".into(),
                participant: "you".into(),
                preview: "secret-preview-should-not-match-search".into(),
                unread: 2,
                order: 0,
                last_at: 0,
                is_group: false,
                writable: true,
                placeholder: false,
                muted: false,
                has_topics: false,
                avatar: Default::default(),
            },
        };
        // A row before Ready is from a cancelled or ended client: dropped.
        snapshot.apply(saved.clone());
        assert!(
            snapshot.conversations.is_empty(),
            "not kept (PR #49 review)"
        );
        assert!(snapshot.visible_conversations().is_empty());
        assert_eq!(snapshot.unread_for(ProtocolId::Telegram), 0);
        complete_telegram(&mut snapshot, &store);
        snapshot.apply(saved);
        assert!(snapshot.has_primary_account());
        assert_eq!(snapshot.selected_protocol, ProtocolId::Telegram);
        assert_eq!(
            snapshot.selected_conversation.as_deref(),
            Some("telegram:saved")
        );
        assert_eq!(snapshot.visible_conversations().len(), 1);
        assert_eq!(snapshot.unread_for(ProtocolId::Telegram), 2);
        assert_eq!(
            store.get(SecretKey::ApiId).expect("id").as_deref(),
            Some("11111")
        );
        assert_eq!(
            store.get(SecretKey::ApiHash).expect("hash").as_deref(),
            Some("hash-value")
        );
        assert_eq!(store.get(SecretKey::Session).expect("session"), None);
        assert_eq!(
            store.get(SecretKey::Phone).expect("phone").as_deref(),
            Some("+15551234567")
        );
        let commands = snapshot.take_commands();
        assert!(commands.iter().any(|c| matches!(
            c,
            AdapterCommand::TelegramAuth {
                step: TelegramAuthStep::ApiCredentials,
                epoch: 0
            }
        )));
        assert!(commands.iter().any(|c| matches!(
            c,
            AdapterCommand::TelegramAuth {
                step: TelegramAuthStep::TwoFactor,
                epoch: 0
            }
        )));
        assert!(!commands.iter().any(|c| matches!(
            c,
            AdapterCommand::TelegramAuth {
                step: TelegramAuthStep::Complete,
                epoch: 0
            }
        )));
        let debug = format!("{commands:?}");
        assert!(!debug.contains("11111"));
        assert!(!debug.contains("hash-value"));
        assert!(!debug.contains("+15551234567"));
        assert!(!debug.contains("12345"));
        assert!(!debug.contains("2fa-secret"));
        assert!(!snapshot.take_keychain_flush());
    }

    #[test]
    fn unavailable_phase_does_not_link_a_live_account() {
        let store = SecretStore::memory();
        seed_override(&store);
        let mut snapshot = Snapshot::new();
        snapshot.open_telegram(&store);
        snapshot.apply(AdapterEvent::TelegramAuth {
            phase: TelegramAuthPhase::NeedPhone,
        });
        snapshot.telegram_phone = "+15551234567".into();
        submit_and_apply(&mut snapshot, &store, TelegramAuthPhase::NeedCode);
        snapshot.telegram_code = "12345".into();
        submit_and_apply(&mut snapshot, &store, TelegramAuthPhase::NeedTwoFactor);
        submit_and_apply(&mut snapshot, &store, TelegramAuthPhase::Unavailable);
        assert_eq!(snapshot.auth, AuthScreen::Idle);
        assert!(!snapshot.has_primary_account());
        assert!(!snapshot.telegram_ready());
        assert!(snapshot.status_text.contains("TDLib unavailable"));
    }

    #[test]
    fn stub_banner_drops_only_on_ready() {
        let store = SecretStore::memory();
        let mut snapshot = Snapshot::new();
        assert!(!snapshot.telegram_ready());
        seed_override(&store);
        snapshot.open_telegram(&store);
        snapshot.apply(AdapterEvent::TelegramAuth {
            phase: TelegramAuthPhase::NeedPhone,
        });
        assert!(!snapshot.telegram_ready());
        snapshot.telegram_phone = "+15551234567".into();
        submit_and_apply(&mut snapshot, &store, TelegramAuthPhase::NeedCode);
        snapshot.telegram_code = "12345".into();
        submit_and_apply(&mut snapshot, &store, TelegramAuthPhase::NeedTwoFactor);
        assert!(!snapshot.telegram_ready());
        submit_and_apply(&mut snapshot, &store, TelegramAuthPhase::Ready);
        assert!(snapshot.telegram_ready());
    }

    #[test]
    fn flush_secrets_event_requests_os_keychain_flush_without_values() {
        let mut snapshot = Snapshot::new();
        assert!(!snapshot.take_keychain_flush());
        snapshot.apply(AdapterEvent::FlushSecrets);
        assert!(snapshot.take_keychain_flush());
        assert!(!snapshot.take_keychain_flush());
        let debug = format!("{:?}", AdapterEvent::FlushSecrets);
        assert!(debug.contains("FlushSecrets"));
        assert!(!debug.to_ascii_lowercase().contains("hash"));
        assert!(!debug.contains("db_key"));
    }

    #[test]
    fn a_failed_keychain_read_blocks_sign_in_and_offers_try_again() {
        let store = SecretStore::detached_for_test();
        store.fail_attach_for_test();
        let mut snapshot =
            Snapshot::with_api_source(TelegramApiSource::with_publisher("11111", "publisher-hash"));
        snapshot.poll_resume(&store);
        assert_eq!(snapshot.center_view(), CenterView::KeychainFailed);

        // No client can start, so no data folder can move (Codex 4091044706).
        snapshot.open_telegram(&store);
        snapshot.open_add_account(&store);
        assert_eq!(snapshot.auth, AuthScreen::Idle);
        assert!(snapshot.take_commands().is_empty());
        assert!(snapshot.error.is_some());

        snapshot.center_key(AuthKey::Enter, &store);
        assert!(snapshot.take_keychain_retry(), "Enter = Try again");
        assert!(!snapshot.take_keychain_retry(), "one retry per press");

        // The retry reads every entry, including a saved session.
        store.complete_ready_attach_for_test(&[
            (SecretKey::ApiId, "11111"),
            (SecretKey::ApiHash, "hash-value"),
            (SecretKey::Session, "tdlib-ready"),
        ]);
        snapshot.try_resume(&store, true);
        snapshot.keychain_failed = store.read_failed();
        assert_eq!(
            snapshot.center_view(),
            CenterView::Resuming { connecting: true }
        );
        assert_eq!(
            auth_steps(&mut snapshot),
            vec![TelegramAuthStep::ApiCredentials]
        );
    }

    #[test]
    fn compose_text_stays_until_the_adapter_accepts_the_send() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        snapshot.compose = "hello".into();
        snapshot.send_compose();
        assert_eq!(send_texts(&mut snapshot), vec!["hello"]);
        assert_eq!(
            snapshot.compose, "hello",
            "not cleared before the pending row"
        );
        assert!(
            !snapshot.can_send(),
            "no second send while one is in flight"
        );
        snapshot.send_compose();
        assert!(send_texts(&mut snapshot).is_empty());

        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(1, 100, "hello", Delivery::Pending),
        });
        accept(&mut snapshot, "telegram:1");
        assert!(
            snapshot.compose.is_empty(),
            "the pending row means accepted"
        );
        assert!(snapshot.error.is_none());
    }

    #[test]
    fn a_send_in_one_chat_does_not_block_send_in_another() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        snapshot.compose = "for Ada".into();
        snapshot.send_compose();
        assert!(!snapshot.can_send(), "chat 1 waits for its pending row");
        snapshot.select_conversation("telegram:2".into());
        snapshot.compose = "for Bob".into();
        assert!(snapshot.can_send(), "chat 2 is free (PR #40 re-review)");
        snapshot.send_compose();
        assert_eq!(send_texts(&mut snapshot), vec!["for Ada", "for Bob"]);

        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(1, 100, "for Ada", Delivery::Pending),
        });
        accept(&mut snapshot, "telegram:1");
        snapshot.select_conversation("telegram:1".into());
        assert!(
            snapshot.compose.is_empty(),
            "chat 1's draft cleared on accept"
        );
        snapshot.select_conversation("telegram:2".into());
        assert_eq!(snapshot.compose, "for Bob", "chat 2 still waits");
    }

    #[test]
    fn an_immediate_send_error_keeps_the_text_and_shows_the_error() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        snapshot.compose = "hello".into();
        snapshot.send_compose();
        let request = sent_request(&mut snapshot);
        // A stale or other request id does not fail this send.
        snapshot.apply(AdapterEvent::SendRejected {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            request: request + 100,
        });
        assert!(!snapshot.can_send(), "still pending");
        snapshot.apply(AdapterEvent::SendRejected {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            request,
        });
        assert_eq!(snapshot.compose, "hello", "no pending row: the text stays");
        let error = snapshot.error.clone().expect("error block");
        assert_eq!(error.happened, "Message not sent.");
        assert!(snapshot.can_send(), "the user can send it again");
    }

    #[test]
    fn an_unrelated_telegram_error_keeps_the_send_pending() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        snapshot.compose = "hello".into();
        snapshot.send_compose();
        snapshot.take_commands();
        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Telegram,
            status: AdapterStatus::Error,
            detail: "Could not load messages (TDLib 500).".into(),
        });
        assert!(
            snapshot.error.is_none(),
            "no \"Message not sent\" for a history error"
        );
        assert!(!snapshot.can_send(), "the send is still pending");
        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(1, 100, "hello", Delivery::Pending),
        });
        accept(&mut snapshot, "telegram:1");
        assert!(snapshot.compose.is_empty(), "then accepted as usual");
    }

    /// The adapter accepts the in-flight send of `chat` (its own request id).
    fn accept(snapshot: &mut Snapshot, chat: &str) {
        let request = snapshot
            .sends
            .request_of(ProtocolId::Telegram, chat)
            .expect("a send in flight");
        snapshot.apply(AdapterEvent::SendAccepted {
            protocol: ProtocolId::Telegram,
            conversation_id: chat.into(),
            request,
        });
    }

    #[test]
    fn a_history_message_with_the_same_text_is_not_an_acceptance() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        snapshot.compose = "ok".into();
        snapshot.send_compose();
        let request = sent_request(&mut snapshot);
        // History (still loading) has an older outgoing "ok" in this chat.
        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(1, 5, "ok", Delivery::Sent),
        });
        assert_eq!(snapshot.compose, "ok", "history does not accept the send");
        assert!(!snapshot.can_send(), "the real send is still in flight");
        // The real send then fails at once: the draft is still there.
        snapshot.apply(AdapterEvent::SendRejected {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            request,
        });
        assert_eq!(snapshot.compose, "ok");
        assert_eq!(
            snapshot.error.clone().expect("error").happened,
            "Message not sent."
        );
    }

    fn sent_request(snapshot: &mut Snapshot) -> u64 {
        snapshot
            .take_commands()
            .into_iter()
            .find_map(|command| match command {
                AdapterCommand::SendText { request, .. } => Some(request),
                _ => None,
            })
            .expect("a SendText")
    }

    #[test]
    fn a_new_phone_step_drops_a_stale_code_and_password() {
        let store = SecretStore::memory();
        let mut snapshot = at_phone_step(&store);
        snapshot.telegram_code = "12345".into();
        snapshot.telegram_2fa = "old-password".into();
        snapshot.apply(AdapterEvent::TelegramAuth {
            phase: TelegramAuthPhase::NeedPhone,
        });
        assert_eq!(snapshot.auth, AuthScreen::TelegramPhone);
        assert!(snapshot.telegram_code.is_empty(), "no stale code");
        assert!(snapshot.telegram_2fa.is_empty(), "no stale password");
    }

    #[test]
    fn exit_waits_until_every_adapter_stopped() {
        let mut snapshot = Snapshot::new();
        assert!(!snapshot.all_stopped());
        snapshot.apply(AdapterEvent::Stopped {
            protocol: ProtocolId::Telegram,
        });
        assert!(
            !snapshot.all_stopped(),
            "Telegram alone is not enough (Codex 4091477244)"
        );
        for protocol in [
            ProtocolId::WhatsApp,
            ProtocolId::Discord,
            ProtocolId::Slack,
            ProtocolId::Signal,
        ] {
            snapshot.apply(AdapterEvent::Stopped { protocol });
        }
        assert!(snapshot.all_stopped());
    }

    #[test]
    fn a_confirmed_missing_key_still_lets_sign_in_start() {
        let store = SecretStore::detached_for_test();
        store.complete_ready_attach_for_test(&[]);
        let mut snapshot =
            Snapshot::with_api_source(TelegramApiSource::with_publisher("11111", "publisher-hash"));
        snapshot.poll_resume(&store);
        assert_ne!(snapshot.center_view(), CenterView::KeychainFailed);
        snapshot.open_telegram(&store);
        assert_eq!(snapshot.auth, AuthScreen::TelegramConnecting);
        assert_eq!(
            auth_steps(&mut snapshot),
            vec![TelegramAuthStep::ApiCredentials]
        );
    }

    #[test]
    fn failed_phase_clears_busy_and_stays_on_the_current_form() {
        let store = SecretStore::memory();
        seed_override(&store);
        let mut snapshot = Snapshot::new();
        snapshot.open_telegram(&store);
        snapshot.apply(AdapterEvent::TelegramAuth {
            phase: TelegramAuthPhase::NeedPhone,
        });
        snapshot.telegram_phone = "+15551234567".into();
        snapshot.advance_telegram(&store);
        assert!(snapshot.auth_busy);
        snapshot.apply(AdapterEvent::TelegramAuth {
            phase: TelegramAuthPhase::Failed,
        });
        assert!(!snapshot.auth_busy);
        assert_eq!(snapshot.auth, AuthScreen::TelegramPhone);
        assert!(snapshot.error.is_some());
        assert!(!snapshot.telegram_ready());
    }

    #[test]
    fn telegram_error_status_clears_auth_busy() {
        let store = SecretStore::memory();
        seed_override(&store);
        let mut snapshot = Snapshot::new();
        snapshot.open_telegram(&store);
        assert!(snapshot.auth_busy);
        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Telegram,
            status: AdapterStatus::Error,
            detail: "telegram api_id must be a number".into(),
        });
        assert!(!snapshot.auth_busy);
        assert_eq!(snapshot.auth, AuthScreen::TelegramConnecting);
        assert!(!snapshot.status_text.contains("11111"));
    }

    #[test]
    fn advanced_override_prefills_from_secret_store() {
        let store = SecretStore::memory();
        store.set(SecretKey::ApiId, "999").expect("set id");
        store
            .set(SecretKey::ApiHash, "stored-hash")
            .expect("set hash");
        let mut snapshot = Snapshot::new();
        snapshot.open_api_override(&store);
        assert_eq!(snapshot.auth, AuthScreen::TelegramApi);
        assert_eq!(snapshot.telegram_api_id, "999");
        assert_eq!(snapshot.telegram_api_hash, "stored-hash");
    }

    #[test]
    fn adapter_status_jargon_does_not_clobber_chrome_status() {
        let mut snapshot = Snapshot::new();
        let before = snapshot.status_text.clone();
        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Slack,
            status: AdapterStatus::Stubbed,
            detail:
                "Official Slack OAuth / workspace app (slack-morphism). Feature slack-oauth is off."
                    .into(),
        });
        assert_eq!(snapshot.status_text, before);
        assert!(!snapshot.status_text.contains("slack-morphism"));
        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Telegram,
            status: AdapterStatus::Connecting,
            detail: "tdlib-rs live client compiled; FFI and network I/O stay off the UI thread"
                .into(),
        });
        assert_eq!(snapshot.status_text, before);
        assert!(!snapshot.status_text.contains("tdlib-rs"));
        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Telegram,
            status: AdapterStatus::Ready,
            detail: "Recent messages loaded.".into(),
        });
        assert_eq!(snapshot.status_text, "Recent messages loaded.");
        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Telegram,
            status: AdapterStatus::Error,
            detail: "telegram api_id must be a number".into(),
        });
        assert_eq!(snapshot.status_text, "telegram api_id must be a number");
    }

    fn discord_guild_placeholder() -> Conversation {
        Conversation {
            protocol: ProtocolId::Discord,
            id: "discord:guild-inbox:general".into(),
            title: "Bot inbox #general".into(),
            participant: "guild channel".into(),
            preview: "placeholder".into(),
            unread: 1,
            order: 0,
            last_at: 0,
            is_group: false,
            writable: true,
            placeholder: true,
            muted: false,
            has_topics: false,
            avatar: Default::default(),
        }
    }

    fn discord_linked(snapshot: &Snapshot) -> bool {
        snapshot
            .accounts
            .iter()
            .find(|row| row.caps.id == ProtocolId::Discord)
            .expect("discord account")
            .linked()
    }

    #[test]
    fn discord_missing_token_placeholder_stays_unlinked() {
        let mut snapshot = Snapshot::new();
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: discord_guild_placeholder(),
        });
        assert!(!discord_linked(&snapshot));
        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Discord,
            status: AdapterStatus::Stubbed,
            detail: "Discord bot inbox placeholder. bot token is not in the OS keychain. Gateway is not started.".into(),
        });
        assert!(!discord_linked(&snapshot));
        snapshot.select_protocol(ProtocolId::Discord);
        assert!(snapshot.visible_conversations().is_empty());
        assert_eq!(snapshot.unread_for(ProtocolId::Discord), 0);
        assert!(!discord_linked(&snapshot));
    }

    #[test]
    fn discord_links_only_when_a_bot_token_is_present() {
        let mut snapshot = Snapshot::new();
        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Discord,
            status: AdapterStatus::Stubbed,
            detail: "Discord bot inbox placeholder. bot token is in the OS keychain. Gateway is not started.".into(),
        });
        assert!(
            !discord_linked(&snapshot),
            "a status never links (shell plan 1)"
        );
        // The adapter links the account, then publishes its rows.
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Discord,
            state: AccountState::Linked,
        });
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: discord_guild_placeholder(),
        });
        assert!(discord_linked(&snapshot));
        if DiscordAdapter::bot_inbox_compiled() {
            assert_eq!(
                snapshot.selected_protocol,
                ProtocolId::Discord,
                "first link"
            );
            assert_eq!(snapshot.visible_conversations().len(), 1);
            assert_eq!(snapshot.unread_for(ProtocolId::Discord), 1);
            assert_eq!(snapshot.center_view(), CenterView::Thread);
        } else {
            assert_eq!(snapshot.selected_protocol, ProtocolId::Telegram);
            assert!(snapshot.visible_conversations().is_empty());
        }
        // A refusal status keeps the link (shell plan 11). Only Account unlinks.
        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Discord,
            status: AdapterStatus::Refused,
            detail: "Discord user-account tokens are refused.".into(),
        });
        assert!(discord_linked(&snapshot));
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Discord,
            state: AccountState::Unlinked,
        });
        assert!(!discord_linked(&snapshot));
        assert_eq!(snapshot.unread_for(ProtocolId::Discord), 0);
    }

    #[test]
    fn discord_visibility_follows_the_compiled_bot_inbox() {
        let mut snapshot = Snapshot::new();
        assert!(!snapshot.telegram_authorized);
        assert!(!snapshot.telegram_ready());
        assert_eq!(
            snapshot.discord_inbox_visible(),
            DiscordAdapter::bot_inbox_compiled()
        );
        assert_eq!(
            snapshot.account_surface_visible(ProtocolId::Discord),
            DiscordAdapter::bot_inbox_compiled()
        );
        assert_eq!(
            snapshot.account_surface_visible(ProtocolId::WhatsApp),
            cfg!(feature = "whatsapp-web")
        );
        snapshot.apply(AdapterEvent::TelegramAuth {
            phase: TelegramAuthPhase::Ready,
        });
        snapshot.apply(AdapterEvent::MessageReceived {
            message: ChatMessage {
                protocol: ProtocolId::Telegram,
                conversation_id: "telegram:1".into(),
                id: "telegram:1:1".into(),
                sender: "worker".into(),
                body: "hello from telegram".into(),
                outbound: false,
                delivery: Delivery::Sent,
                sent_at: 0,
                arrival: thinwire_protocol::Arrival::History,
                topic: None,
                links: Vec::new(),
                photo: None,
                audio: None,
                video: None,
                file: None,
                reactions: Vec::new(),
                reply_to: None,
                edited: false,
            },
        });
        assert_eq!(
            snapshot.discord_inbox_visible(),
            DiscordAdapter::bot_inbox_compiled()
        );
        snapshot.select_protocol(ProtocolId::Discord);
        if DiscordAdapter::bot_inbox_compiled() {
            assert_eq!(snapshot.selected_protocol, ProtocolId::Discord);
        } else {
            assert_eq!(snapshot.selected_protocol, ProtocolId::Telegram);
        }
    }

    #[test]
    fn the_account_sidebar_lists_every_account_of_this_build() {
        let view = Snapshot::new();
        assert_eq!(
            view.shows_in_switcher(ProtocolId::WhatsApp),
            cfg!(feature = "whatsapp-web")
        );
        assert_eq!(
            view.shows_in_switcher(ProtocolId::Discord),
            DiscordAdapter::bot_inbox_compiled()
        );
        assert_eq!(
            view.shows_in_switcher(ProtocolId::Slack),
            cfg!(feature = "slack-oauth")
        );
        assert_eq!(
            view.shows_in_switcher(ProtocolId::Signal),
            cfg!(feature = "signal-local")
        );
        assert!(view.shows_in_switcher(ProtocolId::Telegram));
    }

    #[test]
    fn search_v1_matches_title_and_participant_only() {
        let mut snapshot = Snapshot::new();
        link_telegram(&mut snapshot);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: Conversation {
                protocol: ProtocolId::Telegram,
                id: "telegram:saved".into(),
                title: "Saved Messages".into(),
                participant: "you".into(),
                preview: "secret-preview-should-not-match-search".into(),
                unread: 0,
                order: 0,
                last_at: 0,
                is_group: false,
                writable: true,
                placeholder: false,
                muted: false,
                has_topics: false,
                avatar: Default::default(),
            },
        });
        snapshot
            .accounts
            .iter_mut()
            .find(|row| row.caps.id == ProtocolId::Telegram)
            .expect("telegram row")
            .state = AccountState::Linked;
        snapshot.search = "secret-preview-should-not-match-search".into();
        assert!(snapshot.visible_conversations().is_empty());
        snapshot.search = "you".into();
        assert_eq!(snapshot.visible_conversations().len(), 1);
        snapshot.search = "saved".into();
        assert_eq!(snapshot.visible_conversations().len(), 1);
    }

    #[test]
    fn whatsapp_qr_event_does_not_mark_the_account_ready() {
        let mut snapshot = Snapshot::new();
        snapshot.apply(AdapterEvent::WhatsAppQr {
            code: thinwire_protocol::RedactedPairingSecret::new("qr-do-not-log"),
            generation: 1,
        });
        let row = snapshot
            .accounts
            .iter()
            .find(|row| row.caps.id == ProtocolId::WhatsApp)
            .expect("whatsapp");
        assert_ne!(row.status, AdapterStatus::Ready);
        assert!(!row.linked());
        assert!(!snapshot.status_text.contains("qr-do-not-log"));
        assert!(snapshot.take_commands().is_empty());
        snapshot.select_protocol(ProtocolId::WhatsApp);
        assert!(
            !snapshot
                .take_commands()
                .iter()
                .any(|command| matches!(command, AdapterCommand::WhatsAppBeginLink { .. }))
        );
    }

    #[cfg(feature = "whatsapp-web")]
    #[test]
    fn whatsapp_pairing_does_not_require_a_linked_telegram() {
        let mut snapshot = Snapshot::new();
        assert!(!snapshot.has_primary_account());
        assert!(!snapshot.telegram_ready());
        assert!(snapshot.whatsapp_pairing_available());
        snapshot.open_whatsapp_risk_gate();
        assert_eq!(snapshot.whatsapp_screen, WhatsAppScreen::RiskGate);
        assert!(snapshot.whatsapp_gate_open());
        assert!(snapshot.take_commands().is_empty());
    }

    #[cfg(feature = "whatsapp-web")]
    #[test]
    fn reopening_risk_gate_cancels_an_active_link() {
        let phone = thinwire_protocol::WhatsAppPhoneVault::new();
        let mut snapshot = Snapshot::new();
        snapshot.apply(AdapterEvent::TelegramAuth {
            phase: TelegramAuthPhase::Ready,
        });
        snapshot.open_whatsapp_risk_gate();
        snapshot.acknowledge_whatsapp_risk();
        snapshot.begin_whatsapp_link(&phone);
        assert!(snapshot.whatsapp_started);
        let _ = snapshot.take_commands();
        snapshot.open_whatsapp_risk_gate();
        assert_eq!(snapshot.whatsapp_screen, WhatsAppScreen::RiskGate);
        assert!(!snapshot.whatsapp_started);
        assert!(snapshot.whatsapp_qr.is_none());
        assert!(
            snapshot
                .take_commands()
                .contains(&AdapterCommand::WhatsAppCancelLink)
        );
    }

    #[cfg(feature = "whatsapp-web")]
    #[test]
    fn whatsapp_pair_ui_keeps_phone_off_the_command() {
        let phone = thinwire_protocol::WhatsAppPhoneVault::new();
        let mut snapshot = Snapshot::new();
        assert!(!snapshot.whatsapp_gate_open());
        snapshot.apply(AdapterEvent::TelegramAuth {
            phase: TelegramAuthPhase::Ready,
        });
        snapshot.select_protocol(ProtocolId::WhatsApp);
        assert_eq!(snapshot.whatsapp_screen, WhatsAppScreen::Hidden);
        snapshot.open_whatsapp_risk_gate();
        assert_eq!(snapshot.whatsapp_screen, WhatsAppScreen::RiskGate);
        assert!(snapshot.whatsapp_qr.is_none());
        snapshot.acknowledge_whatsapp_risk();
        assert_eq!(snapshot.whatsapp_screen, WhatsAppScreen::Pair);
        snapshot.whatsapp_phone = "+15559876".into();
        snapshot.begin_whatsapp_link(&phone);
        assert!(snapshot.whatsapp_phone.is_empty());
        assert_eq!(phone.phone().as_deref(), Some("+15559876"));
        let commands = snapshot.take_commands();
        let debug = format!("{commands:?}");
        assert!(!debug.contains("15559876"));
        assert!(commands.contains(&AdapterCommand::WhatsAppAcknowledgeRisk));
        assert!(
            commands
                .iter()
                .any(|command| matches!(command, AdapterCommand::WhatsAppBeginLink { .. }))
        );
        snapshot.apply(AdapterEvent::WhatsAppQr {
            code: thinwire_protocol::RedactedPairingSecret::new("second-secret"),
            generation: 1,
        });
        assert_eq!(snapshot.whatsapp_qr.as_deref(), Some("second-secret"));
        assert!(!snapshot.status_text.contains("second-secret"));
        let row = snapshot
            .accounts
            .iter()
            .find(|row| row.caps.id == ProtocolId::WhatsApp)
            .expect("whatsapp");
        assert!(!row.linked());
        assert_ne!(row.status, AdapterStatus::Ready);
        snapshot.cancel_whatsapp_link(&phone);
        assert_eq!(snapshot.whatsapp_screen, WhatsAppScreen::Hidden);
        assert!(phone.phone().is_none());
        assert!(snapshot.whatsapp_qr.is_none());
    }

    #[test]
    fn send_compose_queues_text_on_the_worker_without_a_local_stub() {
        let mut snapshot = Snapshot::new();
        link_telegram(&mut snapshot);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: telegram_chat(42, "Ada", 1),
        });
        snapshot.selected_protocol = ProtocolId::Telegram;
        snapshot.selected_conversation = Some("telegram:42".into());
        snapshot.compose = " hello ".into();
        snapshot.send_compose();
        assert_eq!(
            snapshot.compose, " hello ",
            "kept until the adapter accepts it"
        );
        assert!(snapshot.selected_messages().is_empty());
        let commands = snapshot.take_commands();
        assert!(commands.iter().any(|command| matches!(
            command,
            AdapterCommand::SendText {
                protocol: ProtocolId::Telegram,
                conversation_id,
                body,
                ..
            } if conversation_id == "telegram:42" && body == "hello"
        )));
        let debug = format!("{commands:?}");
        assert!(debug.contains("hello"));
        assert!(!debug.contains("hash-value"));
    }

    #[test]
    fn send_compose_refuses_before_telegram_is_ready() {
        let mut snapshot = Snapshot::new();
        snapshot.selected_conversation = Some("telegram:42".into());
        snapshot.compose = "hello".into();
        assert!(!snapshot.can_send());
        snapshot.send_compose();
        assert_eq!(snapshot.compose, "hello");
        assert!(snapshot.take_commands().is_empty());
        assert!(
            snapshot.error.is_none(),
            "Send is disabled, so no error block"
        );
        assert!(snapshot.selected_messages().is_empty());
    }

    #[test]
    fn selecting_a_ready_chat_queues_history_and_sorts_by_order() {
        let mut snapshot = Snapshot::new();
        snapshot.telegram_authorized = true;
        snapshot
            .accounts
            .iter_mut()
            .find(|row| row.caps.id == ProtocolId::Telegram)
            .expect("telegram")
            .state = AccountState::Linked;
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: Conversation {
                protocol: ProtocolId::Telegram,
                id: "telegram:2".into(),
                title: "Older".into(),
                participant: "Older".into(),
                preview: "a".into(),
                unread: 0,
                order: 10,
                last_at: 0,
                is_group: false,
                writable: true,
                placeholder: false,
                muted: false,
                has_topics: false,
                avatar: Default::default(),
            },
        });
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: Conversation {
                protocol: ProtocolId::Telegram,
                id: "telegram:9".into(),
                title: "Newer".into(),
                participant: "Newer".into(),
                preview: "b".into(),
                unread: 1,
                order: 90,
                last_at: 0,
                is_group: false,
                writable: true,
                placeholder: false,
                muted: false,
                has_topics: false,
                avatar: Default::default(),
            },
        });
        let ids: Vec<_> = snapshot
            .visible_conversations()
            .iter()
            .map(|row| row.id.as_str())
            .collect();
        assert_eq!(ids, vec!["telegram:9", "telegram:2"]);
        assert_eq!(
            snapshot.selected_conversation.as_deref(),
            Some("telegram:2")
        );
        let commands = snapshot.take_commands();
        assert!(commands.iter().any(|command| matches!(
            command,
            AdapterCommand::OpenChat { conversation_id, .. } if conversation_id == "telegram:2"
        )));
        snapshot.select_conversation("telegram:9".into());
        let commands = snapshot.take_commands();
        assert!(commands.iter().any(|command| matches!(
            command,
            AdapterCommand::OpenChat { conversation_id, .. } if conversation_id == "telegram:9"
        )));
    }

    #[test]
    fn messages_upsert_replace_and_body_edits_keep_sender() {
        let mut snapshot = Snapshot::new();
        link_telegram(&mut snapshot);
        snapshot.selected_protocol = ProtocolId::Telegram;
        snapshot.selected_conversation = Some("telegram:4".into());
        snapshot.apply(AdapterEvent::MessageReceived {
            message: ChatMessage {
                protocol: ProtocolId::Telegram,
                conversation_id: "telegram:4".into(),
                id: "telegram:4:2".into(),
                sender: "Ada".into(),
                body: "second".into(),
                outbound: false,
                delivery: Delivery::Sent,
                sent_at: 0,
                arrival: thinwire_protocol::Arrival::History,
                topic: None,
                links: Vec::new(),
                photo: None,
                audio: None,
                video: None,
                file: None,
                reactions: Vec::new(),
                reply_to: None,
                edited: false,
            },
        });
        snapshot.apply(AdapterEvent::MessageReceived {
            message: ChatMessage {
                protocol: ProtocolId::Telegram,
                conversation_id: "telegram:4".into(),
                id: "telegram:4:1".into(),
                sender: "Ada".into(),
                body: "first".into(),
                outbound: false,
                delivery: Delivery::Sent,
                sent_at: 0,
                arrival: thinwire_protocol::Arrival::History,
                topic: None,
                links: Vec::new(),
                photo: None,
                audio: None,
                video: None,
                file: None,
                reactions: Vec::new(),
                reply_to: None,
                edited: false,
            },
        });
        snapshot.apply(AdapterEvent::MessageReceived {
            message: ChatMessage {
                protocol: ProtocolId::Telegram,
                conversation_id: "telegram:4".into(),
                id: "telegram:4:2".into(),
                sender: "Ada".into(),
                body: "second-edited-via-upsert".into(),
                outbound: false,
                delivery: Delivery::Sent,
                sent_at: 0,
                arrival: thinwire_protocol::Arrival::History,
                topic: None,
                links: Vec::new(),
                photo: None,
                audio: None,
                video: None,
                file: None,
                reactions: Vec::new(),
                reply_to: None,
                edited: false,
            },
        });
        let bodies: Vec<_> = snapshot
            .selected_messages()
            .iter()
            .map(|message| message.body.as_str())
            .collect();
        assert_eq!(bodies, vec!["first", "second-edited-via-upsert"]);
        snapshot.apply(AdapterEvent::MessageReplaced {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:4".into(),
            old_id: "telegram:4:1".into(),
            message: ChatMessage {
                protocol: ProtocolId::Telegram,
                conversation_id: "telegram:4".into(),
                id: "telegram:4:8".into(),
                sender: "you".into(),
                body: "sent".into(),
                outbound: true,
                delivery: Delivery::Sent,
                sent_at: 0,
                arrival: thinwire_protocol::Arrival::History,
                topic: None,
                links: Vec::new(),
                photo: None,
                audio: None,
                video: None,
                file: None,
                reactions: Vec::new(),
                reply_to: None,
                edited: false,
            },
        });
        snapshot.apply(AdapterEvent::MessageBody {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:4".into(),
            message_id: "telegram:4:8".into(),
            body: "sent-edited".into(),
        });
        let messages = snapshot.selected_messages();
        assert!(messages.iter().all(|message| message.id != "telegram:4:1"));
        let edited = messages
            .iter()
            .find(|message| message.id == "telegram:4:8")
            .expect("replaced");
        assert_eq!(edited.body, "sent-edited");
        assert_eq!(edited.sender, "you");
        assert!(edited.outbound);
    }

    #[test]
    fn deleted_message_ids_leave_the_thread() {
        let mut snapshot = Snapshot::new();
        link_telegram(&mut snapshot);
        snapshot.selected_protocol = ProtocolId::Telegram;
        snapshot.selected_conversation = Some("telegram:4".into());
        for (id, body) in [("telegram:4:1", "keep"), ("telegram:4:2", "drop")] {
            snapshot.apply(AdapterEvent::MessageReceived {
                message: ChatMessage {
                    protocol: ProtocolId::Telegram,
                    conversation_id: "telegram:4".into(),
                    id: id.into(),
                    sender: "Ada".into(),
                    body: body.into(),
                    outbound: false,
                    delivery: Delivery::Sent,
                    sent_at: 0,
                    arrival: thinwire_protocol::Arrival::History,
                    topic: None,
                    links: Vec::new(),
                    photo: None,
                    audio: None,
                    video: None,
                    file: None,
                    reactions: Vec::new(),
                    reply_to: None,
                    edited: false,
                },
            });
        }
        snapshot.apply(AdapterEvent::MessagesRemoved {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:4".into(),
            message_ids: vec!["telegram:4:2".into()],
        });
        let bodies: Vec<_> = snapshot
            .selected_messages()
            .iter()
            .map(|message| message.body.as_str())
            .collect();
        assert_eq!(bodies, vec!["keep"]);
    }

    #[test]
    fn removed_chat_drops_messages_and_refresh_reloads_when_ready() {
        let mut snapshot = Snapshot::new();
        snapshot.telegram_authorized = true;
        snapshot
            .accounts
            .iter_mut()
            .find(|row| row.caps.id == ProtocolId::Telegram)
            .expect("telegram")
            .state = AccountState::Linked;
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: Conversation {
                protocol: ProtocolId::Telegram,
                id: "telegram:3".into(),
                title: "Gone".into(),
                participant: "Gone".into(),
                preview: String::new(),
                unread: 2,
                order: 5,
                last_at: 0,
                is_group: false,
                writable: true,
                placeholder: false,
                muted: false,
                has_topics: false,
                avatar: Default::default(),
            },
        });
        snapshot.apply(AdapterEvent::MessageReceived {
            message: ChatMessage {
                protocol: ProtocolId::Telegram,
                conversation_id: "telegram:3".into(),
                id: "telegram:3:1".into(),
                sender: "Ada".into(),
                body: "hi".into(),
                outbound: false,
                delivery: Delivery::Sent,
                sent_at: 0,
                arrival: thinwire_protocol::Arrival::History,
                topic: None,
                links: Vec::new(),
                photo: None,
                audio: None,
                video: None,
                file: None,
                reactions: Vec::new(),
                reply_to: None,
                edited: false,
            },
        });
        let _ = snapshot.take_commands();
        snapshot.apply(AdapterEvent::ConversationRemoved {
            protocol: ProtocolId::Telegram,
            id: "telegram:3".into(),
        });
        assert!(snapshot.visible_conversations().is_empty());
        assert!(snapshot.selected_messages().is_empty());
        snapshot.refresh_visible();
        assert!(snapshot.take_commands().iter().any(|command| matches!(
            command,
            AdapterCommand::LoadChats {
                protocol: ProtocolId::Telegram,
            }
        )));
    }

    #[test]
    fn refresh_does_not_start_an_unlinked_slack_install() {
        let mut snapshot = Snapshot::new();
        snapshot.extra_visible.insert(ProtocolId::Slack);
        snapshot.refresh_visible();
        assert!(
            snapshot.take_commands().iter().all(|command| !matches!(
                command,
                AdapterCommand::Connect {
                    protocol: ProtocolId::Slack,
                }
            )),
            "Refresh must not open the Slack install"
        );
    }

    #[test]
    fn a_slow_keychain_read_asks_the_user_to_unlock() {
        let store = SecretStore::detached_for_test();
        let mut snapshot = Snapshot::new();
        snapshot.poll_resume(&store);
        let started = snapshot.keychain_wait_started.expect("started");
        assert_eq!(snapshot.keychain_wait_text(started), KEYCHAIN_OPENING);
        assert_eq!(
            snapshot.keychain_wait_text(started + KEYCHAIN_SLOW_AFTER),
            KEYCHAIN_WAITING
        );
        snapshot.poll_resume(&store);
        assert_eq!(
            snapshot.keychain_wait_started,
            Some(started),
            "the clock starts once"
        );
    }

    #[test]
    fn a_remote_logout_clears_the_inbox_and_asks_to_sign_in_again() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        snapshot.apply(AdapterEvent::MessageReceived {
            message: telegram_text(1, 7, "hi"),
        });
        snapshot.compose = "draft".into();
        assert!(!snapshot.can_add_account());

        snapshot.apply(AdapterEvent::TelegramSessionEnded);
        assert!(!snapshot.telegram_ready());
        assert!(!snapshot.has_primary_account());
        assert!(
            snapshot.visible_conversations().is_empty(),
            "the old inbox is gone"
        );
        assert!(snapshot.selected_messages().is_empty());
        assert!(snapshot.compose.is_empty());
        assert!(
            snapshot.drafts.is_empty(),
            "a remote logout drops every draft"
        );
        assert!(snapshot.can_add_account(), "Add account is back");
        assert_eq!(snapshot.status_text, SESSION_ENDED_NOTICE);
        assert_eq!(
            snapshot.center_view(),
            CenterView::Resuming { connecting: true }
        );
        assert_eq!(
            auth_steps(&mut snapshot),
            vec![TelegramAuthStep::ApiCredentials]
        );

        snapshot.apply(AdapterEvent::TelegramAuth {
            phase: TelegramAuthPhase::NeedPhone,
        });
        assert_eq!(snapshot.auth, AuthScreen::TelegramPhone);
        assert_eq!(snapshot.auth_notice.as_deref(), Some(SESSION_ENDED_NOTICE));

        snapshot.apply(AdapterEvent::TelegramSessionEnded);
        assert!(
            snapshot.take_commands().is_empty(),
            "a second end does nothing"
        );
    }

    /// The next Telegram account on this machine must not see the previous
    /// account's unsent text, even for the same chat id.
    #[test]
    fn a_relink_after_telegram_logout_shows_an_empty_compose() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        snapshot.compose = "old account".into();
        snapshot.set_draft(
            ProtocolId::Telegram,
            "telegram:2",
            None,
            "other chat".into(),
        );
        snapshot.apply(AdapterEvent::TelegramSessionEnded);
        assert!(snapshot.drafts.is_empty());
        assert!(snapshot.compose.is_empty());

        snapshot.apply(AdapterEvent::TelegramAuth {
            phase: TelegramAuthPhase::Ready,
        });
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: telegram_chat(1, "Ada", 10),
        });
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: telegram_chat(2, "Bob", 5),
        });
        snapshot.select_conversation("telegram:1".into());
        assert!(snapshot.compose.is_empty());
        snapshot.select_conversation("telegram:2".into());
        assert!(snapshot.compose.is_empty());
    }

    #[test]
    fn auth_steps_sees_a_step_with_any_epoch() {
        let mut snapshot = Snapshot::new();
        snapshot.pending.push(AdapterCommand::TelegramAuth {
            step: TelegramAuthStep::Phone,
            epoch: 3,
        });
        assert_eq!(
            auth_steps(&mut snapshot),
            vec![TelegramAuthStep::Phone],
            "a non-zero epoch must not vanish from the helper"
        );
        // The snapshot itself queues epoch 0; the host stamps the real one.
        let store = SecretStore::memory();
        seed_override(&store);
        snapshot.open_telegram(&store);
        assert!(snapshot.take_commands().iter().any(|command| matches!(
            command,
            AdapterCommand::TelegramAuth {
                step: TelegramAuthStep::ApiCredentials,
                epoch: 0
            }
        )));
    }

    #[test]
    fn inbox_events_queued_before_cancel_leave_no_rows() {
        // The worker linked, then the user pressed Cancel before the UI polled
        // Ready. The host drops the stamped Ready. The chat rows and messages
        // queued after it arrive unstamped: the snapshot drops them.
        let store = SecretStore::memory();
        let mut snapshot = Snapshot::new();
        seed_override(&store);
        snapshot.open_telegram(&store);
        snapshot.apply(AdapterEvent::TelegramAuth {
            phase: TelegramAuthPhase::NeedPhone,
        });
        snapshot.cancel_auth(&store);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: telegram_chat(4, "Ada", 9),
        });
        snapshot.apply(AdapterEvent::MessageReceived {
            message: ChatMessage {
                protocol: ProtocolId::Telegram,
                conversation_id: "telegram:4".into(),
                id: "telegram:4:1".into(),
                sender: "Ada".into(),
                body: "from the cancelled account".into(),
                outbound: false,
                delivery: Delivery::Sent,
                sent_at: 0,
                arrival: thinwire_protocol::Arrival::History,
                topic: None,
                links: Vec::new(),
                photo: None,
                audio: None,
                video: None,
                file: None,
                reactions: Vec::new(),
                reply_to: None,
                edited: false,
            },
        });
        snapshot.apply(AdapterEvent::ChatListLoaded {
            protocol: ProtocolId::Telegram,
        });
        assert!(snapshot.conversations.is_empty());
        assert!(snapshot.messages.is_empty());

        // A different account that links later starts with no old rows.
        complete_telegram(&mut snapshot, &store);
        assert!(snapshot.visible_conversations().is_empty());
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: telegram_chat(5, "Bob", 3),
        });
        assert_eq!(snapshot.visible_conversations().len(), 1);
    }

    /// qa L3: Enter on first run and the Add Telegram button take one path.
    /// Signed in but not linked yet, neither opens a second login.
    #[test]
    fn first_run_enter_uses_the_add_account_guard() {
        let store = SecretStore::memory();
        seed_override(&store);
        let mut snapshot = Snapshot::new();
        settle_resume(&mut snapshot);
        snapshot.telegram_authorized = true;
        assert_eq!(snapshot.center_view(), CenterView::FirstRun);
        assert!(!snapshot.can_add_account());
        snapshot.center_key(AuthKey::Enter, &store);
        assert_eq!(snapshot.auth, AuthScreen::Idle);
        assert!(snapshot.take_commands().is_empty());

        snapshot.telegram_authorized = false;
        snapshot.center_key(AuthKey::Enter, &store);
        assert_eq!(snapshot.auth, AuthScreen::TelegramConnecting);
    }

    /// PR #48 review (P1): the core refuses pairing without the ban gate.
    #[cfg(feature = "whatsapp-web")]
    #[test]
    fn whatsapp_pairing_needs_the_risk_gate_in_order() {
        let phone = WhatsAppPhoneVault::new();
        let mut snapshot = Snapshot::new();
        assert!(snapshot.whatsapp_pairing_available());

        // No gate on screen: both steps are dropped.
        snapshot.acknowledge_whatsapp_risk();
        assert_eq!(snapshot.whatsapp_screen, WhatsAppScreen::Hidden);
        snapshot.whatsapp_phone = "15550100".into();
        snapshot.begin_whatsapp_link(&phone);
        assert!(!snapshot.whatsapp_started);
        assert!(snapshot.take_commands().is_empty());

        // Gate shown, not accepted: pairing is still dropped.
        snapshot.open_whatsapp_risk_gate();
        snapshot.begin_whatsapp_link(&phone);
        assert!(!snapshot.whatsapp_started);
        assert!(snapshot.take_commands().is_empty());

        // Normal order works.
        snapshot.acknowledge_whatsapp_risk();
        assert_eq!(snapshot.whatsapp_screen, WhatsAppScreen::Pair);
        snapshot.begin_whatsapp_link(&phone);
        assert!(snapshot.whatsapp_started);
        assert!(matches!(
            snapshot.take_commands().as_slice(),
            [
                AdapterCommand::WhatsAppAcknowledgeRisk,
                AdapterCommand::WhatsAppBeginLink { .. }
            ]
        ));

        // Cancel drops the acknowledgement: a new link needs the gate again.
        snapshot.cancel_whatsapp_link(&phone);
        snapshot.take_commands();
        snapshot.whatsapp_screen = WhatsAppScreen::Pair;
        snapshot.begin_whatsapp_link(&phone);
        assert!(!snapshot.whatsapp_started);
        assert!(snapshot.take_commands().is_empty());
    }

    /// PR #48 review (P1): `Snapshot` is public through `View`, so its
    /// `Debug` must not print a secret or user text.
    #[test]
    fn debug_prints_no_secret_and_no_user_text() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        snapshot.apply(AdapterEvent::MessageReceived {
            message: telegram_text(1, 7, "body-fixture-7c1"),
        });
        snapshot.telegram_api_id = "id-fixture-4410".into();
        snapshot.telegram_api_hash = "hash-fixture-9f3".into();
        snapshot.telegram_phone = "+15550100777".into();
        snapshot.telegram_code = "code-fixture-5521".into();
        snapshot.telegram_2fa = "password-fixture-8d2".into();
        snapshot.compose = "draft-fixture-3b7".into();
        snapshot.search = "search-fixture-6e5".into();
        snapshot.drafts.insert(
            (ProtocolId::Telegram, "telegram:2".into(), None),
            "other-draft-fixture-2a9".into(),
        );
        #[cfg(feature = "whatsapp-web")]
        {
            snapshot.whatsapp_phone = "wa-phone-fixture-111".into();
            snapshot.whatsapp_qr = Some("qr-fixture-222".into());
            snapshot.whatsapp_pair_code = Some("pair-fixture-333".into());
        }
        let shown = format!("{snapshot:?}");
        for secret in [
            "id-fixture-4410",
            "hash-fixture-9f3",
            "5550100777",
            "code-fixture-5521",
            "password-fixture-8d2",
            "draft-fixture-3b7",
            "search-fixture-6e5",
            "other-draft-fixture-2a9",
            "body-fixture-7c1",
            "wa-phone-fixture-111",
            "qr-fixture-222",
            "pair-fixture-333",
        ] {
            assert!(!shown.contains(secret), "{secret} leaked: {shown}");
        }
        assert!(shown.contains("telegram_authorized: true"));
    }

    /// PR #48 review (P2): leaving the pair screen is Cancel, not a hide.
    #[cfg(feature = "whatsapp-web")]
    #[test]
    fn closing_the_pair_screen_cancels_pairing() {
        let phone = WhatsAppPhoneVault::new();
        let mut snapshot = Snapshot::new();
        snapshot.open_whatsapp_risk_gate();
        snapshot.acknowledge_whatsapp_risk();
        snapshot.whatsapp_phone = "15550100".into();
        snapshot.begin_whatsapp_link(&phone);
        assert!(snapshot.whatsapp_started);
        assert!(phone.phone().is_some());
        snapshot.take_commands();

        snapshot.close_whatsapp_gate(&phone);
        assert_eq!(snapshot.whatsapp_screen, WhatsAppScreen::Hidden);
        assert!(!snapshot.whatsapp_started);
        assert!(phone.phone().is_none(), "the phone vault is cleared");
        assert_eq!(
            snapshot.take_commands(),
            vec![AdapterCommand::WhatsAppCancelLink]
        );

        // The acknowledgement is gone: pairing needs the gate again.
        snapshot.whatsapp_screen = WhatsAppScreen::Pair;
        snapshot.begin_whatsapp_link(&phone);
        assert!(!snapshot.whatsapp_started);
        assert!(snapshot.take_commands().is_empty());

        // Back from the gate alone only hides it and sends nothing.
        snapshot.open_whatsapp_risk_gate();
        snapshot.take_commands();
        snapshot.close_whatsapp_gate(&phone);
        assert_eq!(snapshot.whatsapp_screen, WhatsAppScreen::Hidden);
        assert!(snapshot.take_commands().is_empty());
    }

    /// A retry of a failed row, ready for tests: returns its request id.
    fn retry_failed_row(snapshot: &mut Snapshot) -> u64 {
        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(1, 101, "hi", Delivery::Failed),
        });
        snapshot.error = None;
        snapshot.retry_send("telegram:1:101");
        let commands = snapshot.take_commands();
        let request = commands
            .iter()
            .find_map(|command| match command {
                AdapterCommand::ResendMessage { request, .. } => Some(*request),
                _ => None,
            })
            .expect("a ResendMessage");
        assert_eq!(snapshot.selected_messages()[0].delivery, Delivery::Pending);
        request
    }

    /// Shell plan item 10 (Codex #53): a retry stays pending across a
    /// reconnect. Only its own answer ends it.
    #[test]
    fn a_retry_stays_pending_until_its_own_answer() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        let request = retry_failed_row(&mut snapshot);
        for status in [AdapterStatus::Connecting, AdapterStatus::Ready] {
            snapshot.apply(AdapterEvent::Status {
                protocol: ProtocolId::Telegram,
                status,
                detail: "reconnect".into(),
            });
        }
        snapshot.compose = "next".into();
        assert!(!snapshot.can_send(), "the chat still has a retry in flight");
        snapshot.retry_send("telegram:1:101");
        assert!(snapshot.take_commands().is_empty(), "no second retry");
        assert_eq!(snapshot.selected_messages()[0].delivery, Delivery::Pending);

        snapshot.apply(AdapterEvent::SendAccepted {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            request,
        });
        assert!(snapshot.can_send(), "the retry ended");
    }

    /// A rejected retry sets its row back to Failed and shows the error. A
    /// rejection with an old request id changes nothing.
    #[test]
    fn a_rejected_retry_fails_its_row_again() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        let request = retry_failed_row(&mut snapshot);
        snapshot.apply(AdapterEvent::SendRejected {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            request: request + 100,
        });
        assert_eq!(snapshot.selected_messages()[0].delivery, Delivery::Pending);
        assert!(snapshot.error.is_none());

        snapshot.apply(AdapterEvent::SendRejected {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            request,
        });
        assert_eq!(snapshot.selected_messages()[0].delivery, Delivery::Failed);
        assert_eq!(
            snapshot.error.as_ref().map(|error| error.happened.as_str()),
            Some("Message not sent.")
        );
    }

    // region: protocol-independent shell (plan items 1-8, 10, 11)

    /// A Snapshot that shows Slack and Discord too, whatever the features.
    fn shell_with(protocols: &[ProtocolId]) -> Snapshot {
        let mut snapshot = Snapshot::new();
        snapshot.extra_visible.extend(protocols.iter().copied());
        snapshot
    }

    /// Codex r4103183195 on #53: older history pages for any linked protocol
    /// that pages it, not only Telegram. A protocol without paging never asks.
    #[test]
    fn older_history_pages_any_protocol_with_pages_history() {
        let mut snapshot = shell_with(&[ProtocolId::WhatsApp, ProtocolId::Discord]);
        for row in &mut snapshot.accounts {
            if row.caps.id == ProtocolId::WhatsApp {
                row.caps.pages_history = true;
            }
        }
        for (protocol, id) in [
            (ProtocolId::WhatsApp, "whatsapp:111@s.whatsapp.net"),
            (ProtocolId::Discord, "discord:1"),
        ] {
            link(&mut snapshot, protocol);
            snapshot.apply(AdapterEvent::ConversationUpsert {
                conversation: chat(protocol, id, true),
            });
            snapshot.apply(AdapterEvent::MessageReceived {
                message: ChatMessage {
                    protocol,
                    conversation_id: id.into(),
                    id: format!("{id}:m1"),
                    sender: "Ana".into(),
                    body: "hi".into(),
                    outbound: false,
                    delivery: Delivery::Sent,
                    sent_at: 1,
                    arrival: thinwire_protocol::Arrival::History,
                    topic: None,
                    links: Vec::new(),
                    photo: None,
                    audio: None,
                    video: None,
                    file: None,
                    reactions: Vec::new(),
                    reply_to: None,
                    edited: false,
                },
            });
            snapshot.apply(AdapterEvent::HistoryLoaded {
                protocol,
                conversation_id: id.into(),
                store_cursor: None,
            });
        }

        snapshot.select_protocol(ProtocolId::WhatsApp);
        snapshot.select_conversation("whatsapp:111@s.whatsapp.net".into());
        let _ = snapshot.take_commands();
        assert!(snapshot.older_can_ask());
        snapshot.load_older();
        assert!(snapshot.take_commands().iter().any(|command| matches!(
            command,
            AdapterCommand::LoadOlderMessages { protocol: ProtocolId::WhatsApp, before_message_id, .. }
                if before_message_id == "whatsapp:111@s.whatsapp.net:m1"
        )));
        assert_eq!(snapshot.older_state(), OlderState::Loading);
        // The answer ends the request; the start of the chat stops asking.
        snapshot.apply(AdapterEvent::OlderHistoryLoaded {
            protocol: ProtocolId::WhatsApp,
            conversation_id: "whatsapp:111@s.whatsapp.net".into(),
            before_message_id: "whatsapp:111@s.whatsapp.net:m1".into(),
            more: false,
            note: None,
            topic: None,
            store_cursor: None,
        });
        assert_eq!(snapshot.older_state(), OlderState::StartOfChat);
        assert!(!snapshot.older_can_ask());

        snapshot.select_protocol(ProtocolId::Discord);
        snapshot.select_conversation("discord:1".into());
        let _ = snapshot.take_commands();
        assert!(!snapshot.older_can_ask(), "Discord does not page history");
    }

    /// F1 (#82 with #53): the older-page loading line belongs to the
    /// protocol of that page, keyed by (protocol, chat, topic).
    #[test]
    fn older_loading_line_names_only_its_protocol() {
        let mut snapshot = shell_with(&[ProtocolId::WhatsApp]);
        snapshot.older_loading.insert((
            ProtocolId::WhatsApp,
            "whatsapp:1@s.whatsapp.net".into(),
            None,
        ));
        assert_eq!(
            snapshot.loading_line(ProtocolId::WhatsApp),
            Some(LOADING_OLDER_STATUS)
        );
        assert_eq!(snapshot.loading_line(ProtocolId::Telegram), None);
    }

    /// Codex r4103183222 on #53: a failed WhatsApp send names WhatsApp.
    #[test]
    fn failed_delivery_names_the_protocol_of_the_message() {
        let mut snapshot = shell_with(&[ProtocolId::WhatsApp]);
        link(&mut snapshot, ProtocolId::WhatsApp);
        let id = "whatsapp:111@s.whatsapp.net";
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::WhatsApp, id, true),
        });
        snapshot.apply(AdapterEvent::MessageReceived {
            message: ChatMessage {
                protocol: ProtocolId::WhatsApp,
                conversation_id: id.into(),
                id: "pending:1".into(),
                sender: "You".into(),
                body: "hi".into(),
                outbound: true,
                delivery: Delivery::Pending,
                sent_at: 1,
                arrival: thinwire_protocol::Arrival::History,
                topic: None,
                links: Vec::new(),
                photo: None,
                audio: None,
                video: None,
                file: None,
                reactions: Vec::new(),
                reply_to: None,
                edited: false,
            },
        });
        snapshot.apply(AdapterEvent::MessageDelivery {
            protocol: ProtocolId::WhatsApp,
            conversation_id: id.into(),
            message_id: "pending:1".into(),
            delivery: Delivery::Failed,
        });
        let error = snapshot.error.clone().expect("error block");
        assert_eq!(error.why, "WhatsApp did not accept the message.");
    }

    fn link(snapshot: &mut Snapshot, protocol: ProtocolId) {
        snapshot.apply(AdapterEvent::Account {
            protocol,
            state: AccountState::Linked,
        });
    }

    fn chat(protocol: ProtocolId, id: &str, writable: bool) -> Conversation {
        Conversation {
            protocol,
            id: id.into(),
            title: id.into(),
            participant: id.into(),
            preview: String::new(),
            unread: 0,
            order: 1,
            last_at: 0,
            is_group: false,
            writable,
            placeholder: false,
            muted: false,
            has_topics: false,
            avatar: Default::default(),
        }
    }

    fn allow_send(snapshot: &mut Snapshot, protocol: ProtocolId) {
        let row = snapshot
            .accounts
            .iter_mut()
            .find(|row| row.caps.id == protocol)
            .expect("row");
        row.caps.sends_text = true;
    }

    #[test]
    fn a_first_non_telegram_account_shows_its_inbox() {
        let mut snapshot = shell_with(&[ProtocolId::Slack]);
        settle_resume(&mut snapshot);
        assert_eq!(snapshot.center_view(), CenterView::FirstRun);
        link(&mut snapshot, ProtocolId::Slack);
        assert_eq!(
            snapshot.center_view(),
            CenterView::Thread,
            "no Telegram needed"
        );
        assert_eq!(snapshot.selected_protocol, ProtocolId::Slack);

        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Slack, "slack:C1", true),
        });
        assert_eq!(snapshot.selected_conversation.as_deref(), Some("slack:C1"));
        assert!(
            snapshot
                .take_commands()
                .contains(&AdapterCommand::OpenChat {
                    protocol: ProtocolId::Slack,
                    conversation_id: "slack:C1".into(),
                })
        );
        assert_eq!(snapshot.thread_state(), ThreadState::Loading);
        snapshot.apply(AdapterEvent::HistoryLoaded {
            protocol: ProtocolId::Slack,
            conversation_id: "slack:C1".into(),
            store_cursor: None,
        });
        assert_eq!(snapshot.thread_state(), ThreadState::Empty);

        snapshot.refresh_visible();
        assert!(
            snapshot
                .take_commands()
                .contains(&AdapterCommand::LoadChats {
                    protocol: ProtocolId::Slack
                })
        );
        assert_eq!(snapshot.inbox_state(), InboxState::Rows);
    }

    /// The question before an unlink says what happens, for each protocol.
    /// Signal names the place on the phone, because the request to Signal
    /// can fail. No line makes a promise about an unofficial client.
    #[test]
    fn each_protocol_has_an_unlink_notice() {
        let kinds = [
            ProtocolKind::Telegram,
            ProtocolKind::WhatsApp,
            ProtocolKind::Signal,
            ProtocolKind::Discord,
            ProtocolKind::Slack,
        ];
        for kind in kinds {
            let line = unlink_notice(kind);
            assert!(line.ends_with('.'), "{line}");
            assert!(!line.to_ascii_lowercase().contains("reliable"), "{line}");
        }
        assert!(unlink_notice(ProtocolKind::Signal).contains("Settings > Linked devices"));
        assert!(unlink_notice(ProtocolKind::Discord).contains("bot token"));
        assert!(unlink_notice(ProtocolKind::Slack).contains("workspace tokens"));
    }

    #[test]
    fn inbox_events_of_an_unlinked_protocol_are_dropped() {
        let mut snapshot = shell_with(&[ProtocolId::Slack]);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Slack, "slack:C1", true),
        });
        assert!(snapshot.conversations.is_empty());
    }

    #[test]
    fn can_send_checks_the_protocol_and_the_chat() {
        let mut snapshot = shell_with(&[ProtocolId::Slack]);
        link(&mut snapshot, ProtocolId::Slack);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Slack, "slack:RO", false),
        });
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Slack, "slack:RW", true),
        });
        snapshot.compose = "hi".into();
        snapshot.selected_conversation = Some("slack:RW".into());
        assert!(snapshot.can_send(), "a linked writable Slack channel");
        snapshot.selected_conversation = Some("slack:RO".into());
        assert!(!snapshot.can_send(), "a read-only channel");

        snapshot.selected_conversation = Some("slack:RW".into());
        snapshot.take_commands();
        snapshot.send_compose();
        assert!(snapshot.take_commands().iter().any(|command| matches!(
            command,
            AdapterCommand::SendText { protocol: ProtocolId::Slack, conversation_id, .. }
                if conversation_id == "slack:RW"
        )));
        assert!(!snapshot.can_send(), "one send in flight per chat");

        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Slack,
            state: AccountState::Unlinked,
        });
        assert!(!snapshot.can_send(), "unlinked");
    }

    #[test]
    fn a_finished_send_clears_the_sending_line() {
        let mut snapshot = shell_with(&[ProtocolId::Slack]);
        link(&mut snapshot, ProtocolId::Slack);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Slack, "slack:C1", true),
        });
        snapshot.selected_protocol = ProtocolId::Slack;
        snapshot.selected_conversation = Some("slack:C1".into());
        snapshot.history_loading.clear();
        let before = snapshot.status_text.clone();
        snapshot.compose = "hello".into();
        snapshot.send_compose();
        assert_eq!(snapshot.status_line(), SENDING_STATUS);
        let request = snapshot
            .take_commands()
            .into_iter()
            .find_map(|command| match command {
                AdapterCommand::SendText { request, .. } => Some(request),
                _ => None,
            });
        let request = request.expect("send");
        snapshot.apply(AdapterEvent::SendAccepted {
            protocol: ProtocolId::Slack,
            conversation_id: "slack:C1".into(),
            request,
        });
        assert_ne!(snapshot.status_line(), SENDING_STATUS);
        assert_eq!(snapshot.status_text, before);
    }

    #[test]
    fn a_discord_accept_clears_sending_only_when_nothing_is_pending() {
        let mut snapshot = shell_with(&[ProtocolId::Discord]);
        link(&mut snapshot, ProtocolId::Discord);
        allow_send(&mut snapshot, ProtocolId::Discord);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Discord, "discord:1", true),
        });
        snapshot.selected_protocol = ProtocolId::Discord;
        snapshot.selected_conversation = Some("discord:1".into());
        snapshot.history_loading.clear();
        let before = snapshot.status_text.clone();
        snapshot.compose = "hello".into();
        snapshot.send_compose();
        let discord_request = send_request(&mut snapshot);
        snapshot.apply(AdapterEvent::SendAccepted {
            protocol: ProtocolId::Discord,
            conversation_id: "discord:1".into(),
            request: discord_request,
        });
        assert_ne!(snapshot.status_line(), SENDING_STATUS);
        assert_eq!(snapshot.status_text, before);

        link_telegram(&mut snapshot);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: telegram_chat(7, "Ada", 1),
        });
        snapshot.selected_protocol = ProtocolId::Telegram;
        snapshot.selected_conversation = Some("telegram:7".into());
        snapshot.history_loading.clear();
        snapshot.compose = "wait".into();
        snapshot.send_compose();
        let telegram_request = send_request(&mut snapshot);
        snapshot.selected_protocol = ProtocolId::Discord;
        snapshot.selected_conversation = Some("discord:1".into());
        snapshot.compose = "also".into();
        snapshot.send_compose();
        let discord_request = send_request(&mut snapshot);
        snapshot.apply(AdapterEvent::SendAccepted {
            protocol: ProtocolId::Discord,
            conversation_id: "discord:1".into(),
            request: discord_request,
        });
        assert_eq!(
            snapshot.status_line(),
            format!("{}: {SENDING_STATUS}", ProtocolId::Telegram.display_name()),
            "Telegram is still sending"
        );
        snapshot.apply(AdapterEvent::SendAccepted {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:7".into(),
            request: telegram_request,
        });
        assert_eq!(
            snapshot.status_line(),
            format!("{}: {SENDING_STATUS}", ProtocolId::Telegram.display_name()),
            "Telegram's own line has not arrived"
        );
    }

    /// #168 item 16: a send ends on a typed signal, the final delivery of
    /// its row. The Ready text does not matter, and a line alone ends no send.
    #[test]
    fn a_telegram_send_ends_on_its_row_whatever_the_ready_text() {
        let mut snapshot = telegram_ready();
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: telegram_chat(2, "Bea", 2),
        });
        pending_telegram_send(&mut snapshot, 1, 1, "hello");
        assert_eq!(
            snapshot.status_line(),
            SENDING_STATUS,
            "SendAccepted is not the end of the send"
        );
        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Telegram,
            status: AdapterStatus::Ready,
            detail: "Delivered to Ada.".into(),
        });
        assert_eq!(
            snapshot.status_line(),
            SENDING_STATUS,
            "a status line names no send"
        );
        // TDLib's sent message replaces the pending row with a new id.
        snapshot.apply(AdapterEvent::MessageReplaced {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            old_id: "telegram:1:1".into(),
            message: outgoing(1, 5, "hello", Delivery::Sent),
        });
        assert!(snapshot.telegram_send_unconfirmed.is_empty());
        assert_eq!(snapshot.status_line(), "Delivered to Ada.");

        // A delivery change of the bound row is a typed signal too.
        pending_telegram_send(&mut snapshot, 2, 2, "again");
        snapshot.apply(AdapterEvent::MessageDelivery {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:2".into(),
            message_id: "telegram:2:2".into(),
            delivery: Delivery::Sent,
        });
        assert!(snapshot.telegram_send_unconfirmed.is_empty());
        assert_ne!(snapshot.status_line(), SENDING_STATUS);
    }

    #[test]
    fn each_telegram_send_keeps_sending_until_its_own_line() {
        let mut snapshot = Snapshot::new();
        link_telegram(&mut snapshot);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: telegram_chat(1, "Ada", 1),
        });
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: telegram_chat(2, "Bea", 2),
        });
        snapshot.selected_protocol = ProtocolId::Telegram;
        snapshot.selected_conversation = Some("telegram:1".into());
        snapshot.history_loading.clear();
        snapshot.chat_list_loading.clear();
        snapshot.compose = "one".into();
        snapshot.send_compose();
        let first = send_request(&mut snapshot);
        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(1, 1, "one", Delivery::Pending),
        });
        snapshot.selected_conversation = Some("telegram:2".into());
        snapshot.history_loading.clear();
        snapshot.compose = "two".into();
        snapshot.send_compose();
        let second = send_request(&mut snapshot);
        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(2, 2, "two", Delivery::Pending),
        });
        snapshot.apply(AdapterEvent::SendAccepted {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            request: first,
        });
        snapshot.apply(AdapterEvent::SendAccepted {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:2".into(),
            request: second,
        });
        assert_eq!(snapshot.status_line(), SENDING_STATUS);
        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Telegram,
            status: AdapterStatus::Ready,
            detail: "Loading recent messages.".into(),
        });
        assert_eq!(
            snapshot.status_line(),
            SENDING_STATUS,
            "an unrelated Ready line does not clear both sends"
        );
        finish_telegram_send(&mut snapshot, 1, 1, "one");
        assert_eq!(
            snapshot.status_line(),
            SENDING_STATUS,
            "the other send still waits for its own line"
        );
        finish_telegram_send(&mut snapshot, 2, 2, "two");
        assert_eq!(snapshot.status_line(), "Message sent.");
    }

    #[test]
    fn the_second_telegram_send_can_end_first() {
        let mut snapshot = Snapshot::new();
        link_telegram(&mut snapshot);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: telegram_chat(1, "Ada", 1),
        });
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: telegram_chat(2, "Bea", 2),
        });
        snapshot.selected_protocol = ProtocolId::Telegram;
        snapshot.selected_conversation = Some("telegram:1".into());
        snapshot.history_loading.clear();
        snapshot.chat_list_loading.clear();
        snapshot.compose = "one".into();
        snapshot.send_compose();
        let first = send_request(&mut snapshot);
        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(1, 1, "one", Delivery::Pending),
        });
        snapshot.selected_conversation = Some("telegram:2".into());
        snapshot.history_loading.clear();
        snapshot.compose = "two".into();
        snapshot.send_compose();
        let second = send_request(&mut snapshot);
        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(2, 2, "two", Delivery::Pending),
        });
        snapshot.apply(AdapterEvent::SendAccepted {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            request: first,
        });
        snapshot.apply(AdapterEvent::SendAccepted {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:2".into(),
            request: second,
        });
        assert_eq!(snapshot.status_line(), SENDING_STATUS);
        finish_telegram_send(&mut snapshot, 2, 2, "two");
        assert_eq!(
            snapshot.telegram_send_unconfirmed.len(),
            1,
            "the first send stays while the second already ended"
        );
        assert_eq!(snapshot.telegram_send_unconfirmed[0].request, first);
        assert_eq!(snapshot.status_line(), SENDING_STATUS);
        finish_telegram_send(&mut snapshot, 1, 1, "one");
        assert_eq!(snapshot.status_line(), "Message sent.");
    }

    fn finish_telegram_send(snapshot: &mut Snapshot, chat: i64, id: i64, body: &str) {
        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(chat, id, body, Delivery::Sent),
        });
        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Telegram,
            status: AdapterStatus::Ready,
            detail: "Message sent.".into(),
        });
    }

    #[test]
    fn a_telegram_retry_binds_the_replacement_row() {
        let mut snapshot = telegram_ready();
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: telegram_chat(2, "Bea", 2),
        });
        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(1, 1, "one", Delivery::Failed),
        });
        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(2, 2, "two", Delivery::Failed),
        });
        let first = start_telegram_retry(&mut snapshot, "telegram:1", "telegram:1:1");
        snapshot.apply(replacement_pending(1, 1, 11, "one"));
        let second = start_telegram_retry(&mut snapshot, "telegram:2", "telegram:2:2");
        snapshot.apply(replacement_pending(2, 2, 22, "two"));
        snapshot.apply(AdapterEvent::SendAccepted {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            request: first,
        });
        snapshot.apply(AdapterEvent::SendAccepted {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:2".into(),
            request: second,
        });
        assert_eq!(
            hold_message(&snapshot, first).as_deref(),
            Some("telegram:1:11")
        );
        assert_eq!(
            hold_message(&snapshot, second).as_deref(),
            Some("telegram:2:22")
        );
        finish_telegram_send(&mut snapshot, 2, 22, "two");
        assert_eq!(snapshot.telegram_send_unconfirmed.len(), 1);
        assert_eq!(snapshot.telegram_send_unconfirmed[0].request, first);
        assert_ne!(
            snapshot.status_line(),
            SENDING_STATUS,
            "the retry row already shows Sending…"
        );
        finish_telegram_send(&mut snapshot, 1, 11, "one");
        assert!(snapshot.telegram_send_unconfirmed.is_empty());
    }

    #[test]
    fn a_late_telegram_accept_reclaims_its_pending_id() {
        let mut snapshot = telegram_ready();
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: telegram_chat(2, "Bea", 2),
        });
        snapshot.selected_conversation = Some("telegram:1".into());
        snapshot.history_loading.clear();
        snapshot.compose = "one".into();
        snapshot.send_compose();
        let first = send_request(&mut snapshot);
        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(1, 1, "one", Delivery::Pending),
        });
        snapshot.selected_conversation = Some("telegram:2".into());
        snapshot.history_loading.clear();
        snapshot.compose = "two".into();
        snapshot.send_compose();
        let second = send_request(&mut snapshot);
        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(2, 2, "two", Delivery::Pending),
        });
        snapshot.age_sends_for_test(crate::sends::SEND_TIMEOUT);
        assert!(snapshot.expire_sends());
        snapshot.selected_conversation = Some("telegram:1".into());
        snapshot.history_loading.clear();
        snapshot.compose = "newer".into();
        snapshot.send_compose();
        let newer = send_request(&mut snapshot);
        snapshot.apply(AdapterEvent::SendAccepted {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            request: newer,
        });
        assert_eq!(hold_message(&snapshot, newer), None);
        snapshot.apply(AdapterEvent::SendAccepted {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            request: first,
        });
        snapshot.apply(AdapterEvent::SendAccepted {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:2".into(),
            request: second,
        });
        assert_eq!(
            hold_message(&snapshot, first).as_deref(),
            Some("telegram:1:1")
        );
        assert_eq!(
            hold_message(&snapshot, second).as_deref(),
            Some("telegram:2:2")
        );
        finish_telegram_send(&mut snapshot, 2, 2, "two");
        assert_eq!(snapshot.telegram_send_unconfirmed.len(), 2);
        assert!(
            snapshot
                .telegram_send_unconfirmed
                .iter()
                .any(|hold| hold.request == first)
        );
        assert_eq!(snapshot.status_line(), SENDING_STATUS);
    }

    /// #168 item 8: a send ends on its sent row, and its "Message sent."
    /// line never comes. Nothing of that send stays: no hold, no queued id,
    /// no deadline.
    #[test]
    fn a_telegram_send_ends_on_its_row_without_a_sent_line() {
        let mut snapshot = telegram_ready();
        let request = pending_telegram_send(&mut snapshot, 1, 1, "one");
        assert_eq!(
            hold_message(&snapshot, request).as_deref(),
            Some("telegram:1:1")
        );
        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(1, 1, "one", Delivery::Sent),
        });
        assert!(snapshot.telegram_send_unconfirmed.is_empty());
        assert!(snapshot.telegram_unbound_messages.is_empty());
        assert_eq!(snapshot.next_send_deadline(), None);
        assert_ne!(snapshot.status_line(), SENDING_STATUS);
    }

    /// #168 item 17: two sends, and the "Message sent." line of the first
    /// never comes. A later line does not pair with the first send. The
    /// second send, accepted before its row came, ends on its own row.
    #[test]
    fn a_missing_sent_line_does_not_hold_the_next_telegram_send() {
        let mut snapshot = telegram_ready();
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: telegram_chat(2, "Bea", 2),
        });
        pending_telegram_send(&mut snapshot, 1, 1, "one");
        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(1, 1, "one", Delivery::Sent),
        });
        let second = accepted_telegram_send(&mut snapshot, "telegram:2", "two");
        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Telegram,
            status: AdapterStatus::Ready,
            detail: "Message sent.".into(),
        });
        assert_eq!(snapshot.telegram_send_unconfirmed.len(), 1);
        assert_eq!(snapshot.telegram_send_unconfirmed[0].request, second);
        assert_eq!(snapshot.status_line(), SENDING_STATUS);
        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(2, 2, "two", Delivery::Pending),
        });
        assert_eq!(
            hold_message(&snapshot, second).as_deref(),
            Some("telegram:2:2"),
            "the only send of the chat with no row takes the late row"
        );
        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(2, 2, "two", Delivery::Sent),
        });
        assert!(snapshot.telegram_send_unconfirmed.is_empty());
        assert_eq!(snapshot.status_line(), "Message sent.");
    }

    /// #168 item 9: an accepted send of a chat has no row yet, and a newer
    /// send of the same chat is in flight. The newer pending row belongs to
    /// the newer send, not to the older hold.
    #[test]
    fn a_pending_row_binds_to_its_own_telegram_send() {
        let mut snapshot = telegram_ready();
        let older = accepted_telegram_send(&mut snapshot, "telegram:1", "one");
        snapshot.compose = "two".into();
        snapshot.send_compose();
        let newer = send_request(&mut snapshot);
        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(1, 2, "two", Delivery::Pending),
        });
        assert_eq!(hold_message(&snapshot, older), None);
        snapshot.apply(AdapterEvent::SendAccepted {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            request: newer,
        });
        assert_eq!(
            hold_message(&snapshot, newer).as_deref(),
            Some("telegram:1:2")
        );
        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(1, 2, "two", Delivery::Sent),
        });
        assert_eq!(snapshot.telegram_send_unconfirmed.len(), 1);
        assert_eq!(
            snapshot.telegram_send_unconfirmed[0].request, older,
            "the sent row ends its own send only"
        );
    }

    /// #168 item 10: a queued pending id lives as long as the send tracker
    /// keeps its request: open, then expired for `EXPIRED_KEEP`. A send
    /// queues one id only.
    #[test]
    fn a_queued_telegram_id_goes_with_its_request() {
        let mut snapshot = telegram_ready();
        snapshot.selected_protocol = ProtocolId::Telegram;
        snapshot.selected_conversation = Some("telegram:1".into());
        snapshot.compose = "one".into();
        snapshot.send_compose();
        let request = send_request(&mut snapshot);
        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(1, 1, "one", Delivery::Pending),
        });
        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(1, 3, "one", Delivery::Pending),
        });
        assert_eq!(snapshot.telegram_unbound_messages.len(), 1);
        assert_eq!(
            snapshot.telegram_unbound_messages[0].message_id, "telegram:1:3",
            "the newest row of the send wins"
        );
        snapshot.age_sends_for_test(crate::sends::SEND_TIMEOUT);
        assert!(snapshot.expire_sends());
        snapshot.prune_late_answers();
        assert_eq!(
            snapshot.telegram_unbound_messages.len(),
            1,
            "an expired request keeps its id for a late accept"
        );
        snapshot.age_sends_for_test(crate::sends::EXPIRED_KEEP);
        snapshot.prune_late_answers();
        assert!(!snapshot.sends.tracks(ProtocolId::Telegram, request));
        assert!(
            snapshot.telegram_unbound_messages.is_empty(),
            "the id goes with its request"
        );
    }

    /// Codex on #193: the pump that expires a send can come late, for
    /// example after a suspend. The tracker keeps the request for
    /// `EXPIRED_KEEP` from that pump, and the queued id stays as long. A late
    /// accept then binds its row, and the sent row ends the hold.
    #[test]
    fn a_late_accept_after_a_late_expiry_pump_binds_its_row() {
        let mut snapshot = telegram_ready();
        snapshot.selected_protocol = ProtocolId::Telegram;
        snapshot.selected_conversation = Some("telegram:1".into());
        snapshot.compose = "one".into();
        snapshot.send_compose();
        let request = send_request(&mut snapshot);
        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(1, 1, "one", Delivery::Pending),
        });
        // The machine slept for longer than the timeout and the keep window.
        snapshot.age_sends_for_test(crate::sends::SEND_TIMEOUT + crate::sends::EXPIRED_KEEP);
        assert!(snapshot.expire_sends());
        snapshot.prune_late_answers();
        snapshot.apply(AdapterEvent::SendAccepted {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            request,
        });
        assert_eq!(
            hold_message(&snapshot, request).as_deref(),
            Some("telegram:1:1")
        );
        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(1, 1, "one", Delivery::Sent),
        });
        assert!(snapshot.telegram_send_unconfirmed.is_empty());
        assert_ne!(snapshot.status_line(), SENDING_STATUS);
    }

    fn start_telegram_retry(snapshot: &mut Snapshot, chat: &str, message_id: &str) -> u64 {
        snapshot.selected_protocol = ProtocolId::Telegram;
        snapshot.selected_conversation = Some(chat.to_owned());
        snapshot.history_loading.clear();
        snapshot.chat_list_loading.clear();
        snapshot.retry_send(message_id);
        snapshot
            .take_commands()
            .into_iter()
            .find_map(|command| match command {
                AdapterCommand::ResendMessage { request, .. } => Some(request),
                _ => None,
            })
            .expect("retry")
    }

    fn replacement_pending(chat: i64, old: i64, new: i64, body: &str) -> AdapterEvent {
        AdapterEvent::MessageReplaced {
            protocol: ProtocolId::Telegram,
            conversation_id: format!("telegram:{chat}"),
            old_id: format!("telegram:{chat}:{old}"),
            message: outgoing(chat, new, body, Delivery::Pending),
        }
    }

    fn hold_message(snapshot: &Snapshot, request: u64) -> Option<String> {
        snapshot
            .telegram_send_unconfirmed
            .iter()
            .find(|hold| hold.request == request)
            .and_then(|hold| hold.message_id.clone())
    }

    #[test]
    fn a_failed_telegram_delivery_clears_the_sending_line() {
        let mut snapshot = telegram_ready();
        let request = accepted_telegram_send(&mut snapshot, "telegram:1", "one");
        snapshot.apply(AdapterEvent::SendRejected {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            request,
        });
        assert_ne!(snapshot.status_line(), SENDING_STATUS);

        let _request = accepted_telegram_send(&mut snapshot, "telegram:1", "two");
        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(1, 1, "two", Delivery::Pending),
        });
        snapshot.apply(AdapterEvent::MessageDelivery {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            message_id: "telegram:1:1".into(),
            delivery: Delivery::Failed,
        });
        assert_ne!(
            snapshot.status_line(),
            SENDING_STATUS,
            "a failed delivery releases that send"
        );
    }

    #[test]
    fn a_telegram_send_timeout_clears_the_sending_line() {
        let mut snapshot = telegram_ready();
        let _request = accepted_telegram_send(&mut snapshot, "telegram:1", "late");
        snapshot.expire_sends_at(Instant::now() + crate::sends::SEND_TIMEOUT);
        assert_ne!(snapshot.status_line(), SENDING_STATUS);
    }

    #[test]
    fn a_telegram_session_end_clears_the_sending_line() {
        let mut snapshot = telegram_ready();
        let _request = accepted_telegram_send(&mut snapshot, "telegram:1", "bye");
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Telegram,
            state: AccountState::Unlinked,
        });
        assert_ne!(snapshot.status_line(), SENDING_STATUS);
    }

    fn telegram_ready() -> Snapshot {
        let mut snapshot = Snapshot::new();
        link_telegram(&mut snapshot);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: telegram_chat(1, "Ada", 1),
        });
        snapshot.history_loading.clear();
        snapshot.chat_list_loading.clear();
        snapshot.status_text = "Telegram is ready.".into();
        snapshot
    }

    fn accepted_telegram_send(snapshot: &mut Snapshot, chat: &str, body: &str) -> u64 {
        snapshot.selected_protocol = ProtocolId::Telegram;
        snapshot.selected_conversation = Some(chat.to_owned());
        snapshot.history_loading.clear();
        snapshot.chat_list_loading.clear();
        snapshot.compose = body.to_owned();
        snapshot.send_compose();
        let request = send_request(snapshot);
        snapshot.apply(AdapterEvent::SendAccepted {
            protocol: ProtocolId::Telegram,
            conversation_id: chat.to_owned(),
            request,
        });
        assert_eq!(snapshot.status_line(), SENDING_STATUS);
        request
    }

    /// A Telegram send in the order of the adapter: the pending row, then
    /// `SendAccepted`. Returns the request.
    fn pending_telegram_send(snapshot: &mut Snapshot, chat: i64, id: i64, body: &str) -> u64 {
        let conversation_id = format!("telegram:{chat}");
        snapshot.selected_protocol = ProtocolId::Telegram;
        snapshot.selected_conversation = Some(conversation_id.clone());
        snapshot.history_loading.clear();
        snapshot.chat_list_loading.clear();
        snapshot.compose = body.to_owned();
        snapshot.send_compose();
        let request = send_request(snapshot);
        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(chat, id, body, Delivery::Pending),
        });
        snapshot.apply(AdapterEvent::SendAccepted {
            protocol: ProtocolId::Telegram,
            conversation_id,
            request,
        });
        request
    }

    #[test]
    fn a_discord_accept_keeps_telegram_sending_until_its_row_is_sent() {
        let mut snapshot = shell_with(&[ProtocolId::Discord]);
        link(&mut snapshot, ProtocolId::Discord);
        allow_send(&mut snapshot, ProtocolId::Discord);
        link_telegram(&mut snapshot);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Discord, "discord:1", true),
        });
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: telegram_chat(7, "Ada", 1),
        });
        snapshot.history_loading.clear();

        snapshot.selected_protocol = ProtocolId::Telegram;
        snapshot.selected_conversation = Some("telegram:7".into());
        snapshot.compose = "from telegram".into();
        snapshot.send_compose();
        let telegram_request = send_request(&mut snapshot);
        snapshot.selected_protocol = ProtocolId::Discord;
        snapshot.selected_conversation = Some("discord:1".into());
        snapshot.compose = "from discord".into();
        snapshot.send_compose();
        let discord_request = send_request(&mut snapshot);

        snapshot.apply(AdapterEvent::SendAccepted {
            protocol: ProtocolId::Discord,
            conversation_id: "discord:1".into(),
            request: discord_request,
        });
        assert_eq!(
            snapshot.status_line(),
            format!("{}: {SENDING_STATUS}", ProtocolId::Telegram.display_name()),
            "Discord accepted first; Telegram is still sending"
        );

        snapshot.apply(AdapterEvent::MessageReceived {
            message: outgoing(7, 1, "from telegram", Delivery::Pending),
        });
        snapshot.apply(AdapterEvent::SendAccepted {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:7".into(),
            request: telegram_request,
        });
        snapshot.compose = "again".into();
        snapshot.send_compose();
        let later_discord = send_request(&mut snapshot);
        snapshot.apply(AdapterEvent::SendAccepted {
            protocol: ProtocolId::Discord,
            conversation_id: "discord:1".into(),
            request: later_discord,
        });
        assert_eq!(
            snapshot.status_line(),
            format!("{}: {SENDING_STATUS}", ProtocolId::Telegram.display_name()),
            "Telegram accepted, and its sent row has not arrived"
        );
        finish_telegram_send(&mut snapshot, 7, 1, "from telegram");
        assert_eq!(snapshot.status_line(), "Message sent.");
    }

    fn send_request(snapshot: &mut Snapshot) -> u64 {
        snapshot
            .take_commands()
            .into_iter()
            .find_map(|command| match command {
                AdapterCommand::SendText { request, .. } => Some(request),
                _ => None,
            })
            .expect("send")
    }

    #[test]
    fn send_answers_work_per_protocol() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        snapshot.extra_visible.insert(ProtocolId::Discord);
        link(&mut snapshot, ProtocolId::Discord);
        allow_send(&mut snapshot, ProtocolId::Discord);
        // The same chat id in two protocols.
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Discord, "telegram:1", true),
        });
        snapshot.select_protocol(ProtocolId::Discord);
        snapshot.select_conversation("telegram:1".into());
        snapshot.compose = "to discord".into();
        snapshot.send_compose();
        let request = snapshot
            .sends
            .request_of(ProtocolId::Discord, "telegram:1")
            .expect("discord send");

        // An answer for Telegram's chat with the same id changes nothing.
        snapshot.apply(AdapterEvent::SendAccepted {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            request,
        });
        assert_eq!(snapshot.compose, "to discord");
        snapshot.apply(AdapterEvent::SendAccepted {
            protocol: ProtocolId::Discord,
            conversation_id: "telegram:1".into(),
            request,
        });
        assert!(snapshot.compose.is_empty(), "Discord accepted its own send");
    }

    #[test]
    fn retry_works_for_any_linked_protocol() {
        let mut snapshot = shell_with(&[ProtocolId::Discord]);
        link(&mut snapshot, ProtocolId::Discord);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Discord, "discord:9", true),
        });
        snapshot.apply(AdapterEvent::MessageReceived {
            message: ChatMessage {
                protocol: ProtocolId::Discord,
                conversation_id: "discord:9".into(),
                id: "discord:9:1".into(),
                sender: "you".into(),
                body: "hi".into(),
                outbound: true,
                delivery: Delivery::Failed,
                sent_at: 1,
                arrival: thinwire_protocol::Arrival::History,
                topic: None,
                links: Vec::new(),
                photo: None,
                audio: None,
                video: None,
                file: None,
                reactions: Vec::new(),
                reply_to: None,
                edited: false,
            },
        });
        snapshot.take_commands();
        let already_sends = snapshot
            .accounts
            .iter()
            .any(|row| row.caps.id == ProtocolId::Discord && row.caps.sends_text);
        if !already_sends {
            snapshot.retry_send("discord:9:1");
            assert!(
                snapshot.take_commands().is_empty(),
                "Discord does not send text here"
            );
            allow_send(&mut snapshot, ProtocolId::Discord);
        }
        snapshot.retry_send("discord:9:1");
        assert!(snapshot.take_commands().iter().any(|command| matches!(
            command,
            AdapterCommand::ResendMessage { protocol: ProtocolId::Discord, message_id, .. }
                if message_id == "discord:9:1"
        )));
    }

    /// PR #68 review: no Retry in a read-only chat.
    #[test]
    fn retry_is_refused_in_a_read_only_chat() {
        let mut snapshot = shell_with(&[ProtocolId::Discord]);
        link(&mut snapshot, ProtocolId::Discord);
        allow_send(&mut snapshot, ProtocolId::Discord);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Discord, "discord:ro", false),
        });
        snapshot.apply(AdapterEvent::MessageReceived {
            message: ChatMessage {
                protocol: ProtocolId::Discord,
                conversation_id: "discord:ro".into(),
                id: "discord:ro:1".into(),
                sender: "you".into(),
                body: "hi".into(),
                outbound: true,
                delivery: Delivery::Failed,
                sent_at: 1,
                arrival: thinwire_protocol::Arrival::History,
                topic: None,
                links: Vec::new(),
                photo: None,
                audio: None,
                video: None,
                file: None,
                reactions: Vec::new(),
                reply_to: None,
                edited: false,
            },
        });
        snapshot.take_commands();
        snapshot.retry_send("discord:ro:1");
        assert!(snapshot.take_commands().is_empty());
        assert_eq!(snapshot.selected_messages()[0].delivery, Delivery::Failed);
    }

    #[test]
    fn a_failed_command_keeps_the_inbox_and_the_send() {
        let mut snapshot = shell_with(&[ProtocolId::Slack]);
        link(&mut snapshot, ProtocolId::Slack);
        allow_send(&mut snapshot, ProtocolId::Slack);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Slack, "slack:C1", true),
        });
        snapshot.compose = "hi".into();
        snapshot.send_compose();
        snapshot.refresh_visible();
        assert_eq!(snapshot.thread_state(), ThreadState::Loading);

        snapshot.apply(AdapterEvent::CommandFailed {
            protocol: ProtocolId::Slack,
            conversation_id: Some("slack:C1".into()),
            detail: "rate limited".into(),
        });
        assert!(snapshot.protocol_linked(ProtocolId::Slack));
        assert_eq!(snapshot.visible_conversations().len(), 1);
        assert_eq!(snapshot.selected_conversation.as_deref(), Some("slack:C1"));
        assert_eq!(
            snapshot.thread_state(),
            ThreadState::Empty,
            "spinner stopped"
        );
        assert!(snapshot.error.is_some());
        assert!(
            snapshot.sends.in_flight(ProtocolId::Slack, "slack:C1"),
            "only SendRejected ends a send"
        );
    }

    /// Codex #51: a notice is a note, not a refusal.
    #[test]
    fn a_notice_is_a_note_and_never_ends_a_send() {
        let mut snapshot = shell_with(&[ProtocolId::Discord]);
        link(&mut snapshot, ProtocolId::Discord);
        allow_send(&mut snapshot, ProtocolId::Discord);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Discord, "discord:9", true),
        });
        snapshot.compose = "hi".into();
        snapshot.send_compose();
        snapshot.apply(AdapterEvent::Notice {
            protocol: ProtocolId::Discord,
            text: "The bot cannot read this channel.".into(),
        });
        assert!(snapshot.error.is_none());
        assert!(snapshot.sends.in_flight(ProtocolId::Discord, "discord:9"));
        assert_eq!(
            snapshot.notice(ProtocolId::Discord),
            Some("The bot cannot read this channel.")
        );
        assert_eq!(snapshot.notice(ProtocolId::Telegram), None);
    }

    /// Plan item 11 (Codex #47): an error status never unlinks.
    #[test]
    fn an_error_status_never_unlinks() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        snapshot.extra_visible.insert(ProtocolId::Slack);
        link(&mut snapshot, ProtocolId::Slack);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Slack, "slack:C1", true),
        });
        for protocol in [ProtocolId::Telegram, ProtocolId::Slack] {
            snapshot.apply(AdapterEvent::Status {
                protocol,
                status: AdapterStatus::Error,
                detail: "network down".into(),
            });
            assert!(snapshot.protocol_linked(protocol), "{protocol}");
        }
        assert_eq!(snapshot.center_view(), CenterView::Thread);
        assert_eq!(
            snapshot.visible_conversations().len(),
            2,
            "Telegram rows stay"
        );
    }

    #[test]
    fn unlinking_one_protocol_drops_only_its_state() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        snapshot.extra_visible.insert(ProtocolId::Slack);
        link(&mut snapshot, ProtocolId::Slack);
        allow_send(&mut snapshot, ProtocolId::Slack);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Slack, "slack:C1", true),
        });
        snapshot.select_protocol(ProtocolId::Slack);
        snapshot.compose = "slack text".into();
        snapshot.send_compose();
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Slack,
            state: AccountState::Unlinked,
        });
        assert!(!snapshot.conversations.contains_key(&ProtocolId::Slack));
        assert!(!snapshot.sends.in_flight(ProtocolId::Slack, "slack:C1"));
        assert_eq!(
            snapshot.selected_protocol,
            ProtocolId::Telegram,
            "the next linked protocol takes the selection"
        );
        assert_eq!(snapshot.visible_conversations().len(), 2);
        assert!(snapshot.protocol_linked(ProtocolId::Telegram));
    }

    // endregion: protocol-independent shell

    // region: viewed chat (plan item 9)

    fn view_commands(snapshot: &mut Snapshot) -> Vec<(ProtocolId, Option<String>)> {
        snapshot.sync_viewed();
        snapshot
            .take_commands()
            .into_iter()
            .filter_map(|command| match command {
                AdapterCommand::ViewChat {
                    protocol,
                    conversation_id,
                } => Some((protocol, conversation_id)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn view_chat_follows_the_selection_and_sends_only_changes() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        snapshot.extra_visible.insert(ProtocolId::Slack);
        assert_eq!(
            view_commands(&mut snapshot),
            vec![(ProtocolId::Telegram, Some("telegram:1".into()))]
        );
        assert!(
            view_commands(&mut snapshot).is_empty(),
            "no change, no command"
        );

        snapshot.select_conversation("telegram:2".into());
        assert_eq!(
            view_commands(&mut snapshot),
            vec![(ProtocolId::Telegram, Some("telegram:2".into()))]
        );
        snapshot.select_conversation("telegram:2".into());
        assert!(
            view_commands(&mut snapshot).is_empty(),
            "the same chat again"
        );

        link(&mut snapshot, ProtocolId::Slack);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Slack, "slack:C1", true),
        });
        snapshot.select_protocol(ProtocolId::Slack);
        assert_eq!(
            view_commands(&mut snapshot),
            vec![
                (ProtocolId::Telegram, None),
                (ProtocolId::Slack, Some("slack:C1".into()))
            ],
            "the old protocol hears None first"
        );

        snapshot.apply(AdapterEvent::ConversationRemoved {
            protocol: ProtocolId::Slack,
            id: "slack:C1".into(),
        });
        assert_eq!(
            view_commands(&mut snapshot),
            vec![(ProtocolId::Slack, None)]
        );
    }

    #[test]
    fn view_chat_clears_on_session_end_and_under_a_login_form() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        view_commands(&mut snapshot);
        // Advanced opens a form over the thread (only while not signed in,
        // so use the auth screen directly).
        snapshot.auth = AuthScreen::TelegramApi;
        assert_eq!(
            view_commands(&mut snapshot),
            vec![(ProtocolId::Telegram, None)]
        );
        snapshot.auth = AuthScreen::Idle;
        assert_eq!(
            view_commands(&mut snapshot),
            vec![(ProtocolId::Telegram, Some("telegram:1".into()))]
        );
        snapshot.apply(AdapterEvent::TelegramSessionEnded);
        assert_eq!(
            view_commands(&mut snapshot),
            vec![(ProtocolId::Telegram, None)]
        );
    }

    /// WhatsApp and Slack: a live message in the viewed chat upserts unread 0.
    /// A chat they were told is not viewed keeps a badge.
    struct ViewingFake {
        open: Option<String>,
    }

    impl ViewingFake {
        fn apply(&mut self, commands: &[(ProtocolId, Option<String>)]) {
            for (_, id) in commands {
                self.open.clone_from(id);
            }
        }

        fn live_unread(&self, chat: &str) -> u32 {
            if self.open.as_deref() == Some(chat) {
                0
            } else {
                3
            }
        }
    }

    /// Codex P1 on #177: with the window blurred, a live message in the open
    /// chat keeps unread above 0. The count comes from a fake adapter that
    /// zeros only the chat `ViewChat` still names. Focus clears the badge.
    #[test]
    fn blur_keeps_a_fake_adapters_unread_until_focus() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        let mut fake = ViewingFake { open: None };
        let opened = view_commands(&mut snapshot);
        assert_eq!(
            opened,
            vec![(ProtocolId::Telegram, Some("telegram:1".into()))]
        );
        fake.apply(&opened);

        snapshot.set_window_focus(false);
        let blurred = view_commands(&mut snapshot);
        assert_eq!(
            blurred,
            vec![(ProtocolId::Telegram, None)],
            "blur tells adapters the chat is not viewed"
        );
        fake.apply(&blurred);
        assert!(snapshot.viewed().is_none());

        let unread = fake.live_unread("telegram:1");
        assert!(unread > 0, "the fake keeps a badge while blurred");
        let mut row = telegram_chat(1, "Ada", 1);
        row.unread = unread;
        snapshot.apply(AdapterEvent::ConversationUpsert { conversation: row });
        assert_eq!(snapshot.unread_for(ProtocolId::Telegram), unread);
        assert!(
            view_commands(&mut snapshot).is_empty(),
            "the upsert does not change the view"
        );

        snapshot.set_window_focus(true);
        let focused = view_commands(&mut snapshot);
        assert_eq!(
            focused,
            vec![(ProtocolId::Telegram, Some("telegram:1".into()))],
            "focus names the open chat again"
        );
        fake.apply(&focused);
        assert_eq!(fake.live_unread("telegram:1"), 0);
        assert_eq!(snapshot.unread_for(ProtocolId::Telegram), 0);
    }

    // endregion: viewed chat

    /// Plan item 12 (Codex #53): only the current pairing's payloads show.
    #[cfg(feature = "whatsapp-web")]
    #[test]
    fn pairing_payloads_of_an_older_pairing_are_dropped() {
        use thinwire_protocol::RedactedPairingSecret;

        let phone = WhatsAppPhoneVault::new();
        let mut snapshot = Snapshot::new();
        let begin = |snapshot: &mut Snapshot| {
            snapshot.open_whatsapp_risk_gate();
            snapshot.acknowledge_whatsapp_risk();
            snapshot.begin_whatsapp_link(&phone);
            snapshot
                .take_commands()
                .into_iter()
                .find_map(|command| match command {
                    AdapterCommand::WhatsAppBeginLink { generation } => Some(generation),
                    _ => None,
                })
                .expect("begin")
        };
        let qr = |generation: u64, code: &str| AdapterEvent::WhatsAppQr {
            code: RedactedPairingSecret::new(code),
            generation,
        };
        let first = begin(&mut snapshot);
        snapshot.cancel_whatsapp_link(&phone);
        let second = begin(&mut snapshot);
        assert!(second > first, "the generation rises with each begin");

        snapshot.apply(qr(first, "old-qr"));
        assert_eq!(
            snapshot.whatsapp_qr, None,
            "a late payload of the cancelled pairing"
        );
        snapshot.apply(qr(second, "new-qr"));
        assert_eq!(snapshot.whatsapp_qr.as_deref(), Some("new-qr"));

        snapshot.cancel_whatsapp_link(&phone);
        snapshot.apply(AdapterEvent::WhatsAppPairCode {
            code: RedactedPairingSecret::new("late-code"),
            generation: second,
        });
        assert_eq!(snapshot.whatsapp_pair_code, None, "no pairing runs");
        assert!(!format!("{snapshot:?}").contains("new-qr"));
    }

    // region: review fixes on #68 (qa M1, Codex P2)

    /// qa M1: a reconnect (`Linked` then `Linking`) keeps the session, and
    /// the answer of a send in flight still applies.
    #[test]
    fn a_reconnect_keeps_the_session_and_applies_send_answers() {
        let mut snapshot = shell_with(&[ProtocolId::Slack]);
        link(&mut snapshot, ProtocolId::Slack);
        allow_send(&mut snapshot, ProtocolId::Slack);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Slack, "slack:C1", true),
        });
        snapshot.compose = "hi".into();
        snapshot.send_compose();
        let request = snapshot
            .sends
            .request_of(ProtocolId::Slack, "slack:C1")
            .expect("send");

        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Slack,
            state: AccountState::Linking,
        });
        assert_eq!(snapshot.visible_conversations().len(), 1, "rows stay");
        assert_eq!(snapshot.center_view(), CenterView::Thread);
        snapshot.apply(AdapterEvent::SendAccepted {
            protocol: ProtocolId::Slack,
            conversation_id: "slack:C1".into(),
            request,
        });
        assert!(
            snapshot.compose.is_empty(),
            "the answer applied while Linking"
        );
        assert!(!snapshot.sends.in_flight(ProtocolId::Slack, "slack:C1"));
        snapshot.compose = "next".into();
        assert!(!snapshot.can_send(), "new sends wait for Linked");

        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Slack,
            state: AccountState::Linked,
        });
        assert!(snapshot.can_send());
    }

    /// qa M1: `Linking` then `Unlinked` ends the session: rows and sends go.
    #[test]
    fn unlinked_after_linking_ends_the_session() {
        let mut snapshot = shell_with(&[ProtocolId::Slack]);
        link(&mut snapshot, ProtocolId::Slack);
        allow_send(&mut snapshot, ProtocolId::Slack);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Slack, "slack:C1", true),
        });
        snapshot.compose = "hi".into();
        snapshot.send_compose();
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Slack,
            state: AccountState::Linking,
        });
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Slack,
            state: AccountState::Unlinked,
        });
        assert!(!snapshot.conversations.contains_key(&ProtocolId::Slack));
        assert!(!snapshot.sends.in_flight(ProtocolId::Slack, "slack:C1"));
        assert!(!snapshot.has_session(ProtocolId::Slack));

        // A first login (`Linking` with no session before) drops inbox events.
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Slack,
            state: AccountState::Linking,
        });
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Slack, "slack:C2", true),
        });
        assert!(!snapshot.conversations.contains_key(&ProtocolId::Slack));
    }

    /// A 401 rejects the send and then unlinks. The text stays in the chat
    /// draft so the user can send it again after replacing the token.
    #[test]
    fn a_rejected_send_keeps_its_text_after_unlink() {
        let mut snapshot = shell_with(&[ProtocolId::Discord]);
        link(&mut snapshot, ProtocolId::Discord);
        allow_send(&mut snapshot, ProtocolId::Discord);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Discord, "discord:1:2", true),
        });
        snapshot.selected_conversation = Some("discord:1:2".into());
        snapshot.compose = "keep me".into();
        snapshot.set_draft(
            ProtocolId::Discord,
            "discord:9:9",
            None,
            "other chat".into(),
        );
        snapshot.send_compose();
        let request = snapshot
            .take_commands()
            .into_iter()
            .find_map(|command| match command {
                AdapterCommand::SendText { request, .. } => Some(request),
                _ => None,
            })
            .expect("send");
        snapshot.apply(AdapterEvent::SendRejected {
            protocol: ProtocolId::Discord,
            conversation_id: "discord:1:2".into(),
            request,
        });
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Discord,
            state: AccountState::Unlinked,
        });
        assert_eq!(
            snapshot
                .drafts
                .get(&(ProtocolId::Discord, "discord:1:2".into(), None))
                .map(String::as_str),
            Some("keep me"),
        );
        assert!(
            !snapshot
                .drafts
                .contains_key(&(ProtocolId::Discord, "discord:9:9".into(), None)),
            "only the rejected send stays"
        );
        link(&mut snapshot, ProtocolId::Discord);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Discord, "discord:1:2", true),
        });
        snapshot.select_conversation("discord:1:2".into());
        assert_eq!(snapshot.compose, "keep me");
    }

    /// A later accepted send of the same text must not come back on unlink.
    #[test]
    fn an_accepted_resend_drops_the_rejected_body() {
        let mut snapshot = shell_with(&[ProtocolId::Discord]);
        link(&mut snapshot, ProtocolId::Discord);
        allow_send(&mut snapshot, ProtocolId::Discord);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Discord, "discord:1:2", true),
        });
        snapshot.selected_conversation = Some("discord:1:2".into());
        snapshot.compose = "keep me".into();
        snapshot.send_compose();
        let request = snapshot
            .take_commands()
            .into_iter()
            .find_map(|command| match command {
                AdapterCommand::SendText { request, .. } => Some(request),
                _ => None,
            })
            .expect("send");
        snapshot.apply(AdapterEvent::SendRejected {
            protocol: ProtocolId::Discord,
            conversation_id: "discord:1:2".into(),
            request,
        });
        snapshot.send_compose();
        let again = snapshot
            .take_commands()
            .into_iter()
            .find_map(|command| match command {
                AdapterCommand::SendText { request, .. } => Some(request),
                _ => None,
            })
            .expect("resend");
        snapshot.apply(AdapterEvent::SendAccepted {
            protocol: ProtocolId::Discord,
            conversation_id: "discord:1:2".into(),
            request: again,
        });
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Discord,
            state: AccountState::Unlinked,
        });
        link(&mut snapshot, ProtocolId::Discord);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Discord, "discord:1:2", true),
        });
        snapshot.select_conversation("discord:1:2".into());
        assert!(snapshot.compose.is_empty());
    }

    /// A 401 on a retry unlinks. The failed row's text stays in the draft
    /// when compose has nothing newer.
    #[test]
    fn a_rejected_retry_keeps_the_row_text_after_unlink() {
        let mut snapshot = shell_with(&[ProtocolId::Discord]);
        link(&mut snapshot, ProtocolId::Discord);
        allow_send(&mut snapshot, ProtocolId::Discord);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Discord, "discord:1:2", true),
        });
        snapshot.selected_conversation = Some("discord:1:2".into());
        snapshot.apply(AdapterEvent::MessageReceived {
            message: ChatMessage {
                protocol: ProtocolId::Discord,
                conversation_id: "discord:1:2".into(),
                id: "discord:pending:2:1".into(),
                sender: "bot".into(),
                body: "keep me".into(),
                outbound: true,
                delivery: Delivery::Failed,
                sent_at: 1,
                arrival: thinwire_protocol::Arrival::History,
                topic: None,
                links: Vec::new(),
                photo: None,
                audio: None,
                video: None,
                file: None,
                reactions: Vec::new(),
                reply_to: None,
                edited: false,
            },
        });
        snapshot.retry_send("discord:pending:2:1");
        let request = snapshot
            .take_commands()
            .into_iter()
            .find_map(|command| match command {
                AdapterCommand::ResendMessage { request, .. } => Some(request),
                _ => None,
            })
            .expect("retry");
        snapshot.apply(AdapterEvent::SendRejected {
            protocol: ProtocolId::Discord,
            conversation_id: "discord:1:2".into(),
            request,
        });
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Discord,
            state: AccountState::Unlinked,
        });
        link(&mut snapshot, ProtocolId::Discord);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Discord, "discord:1:2", true),
        });
        snapshot.select_conversation("discord:1:2".into());
        assert_eq!(snapshot.compose, "keep me");
    }

    /// Send 'A' is rejected and stays in compose. A later retry of row 'B'
    /// that gets 401 must not replace 'A'.
    #[test]
    fn a_rejected_compose_survives_a_retry_that_unlinks() {
        let mut snapshot = shell_with(&[ProtocolId::Discord]);
        link(&mut snapshot, ProtocolId::Discord);
        allow_send(&mut snapshot, ProtocolId::Discord);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Discord, "discord:1:2", true),
        });
        snapshot.selected_conversation = Some("discord:1:2".into());
        snapshot.compose = "A".into();
        snapshot.send_compose();
        let request = snapshot
            .take_commands()
            .into_iter()
            .find_map(|command| match command {
                AdapterCommand::SendText { request, .. } => Some(request),
                _ => None,
            })
            .expect("send");
        snapshot.apply(AdapterEvent::SendRejected {
            protocol: ProtocolId::Discord,
            conversation_id: "discord:1:2".into(),
            request,
        });
        assert_eq!(snapshot.compose, "A");
        snapshot.apply(AdapterEvent::MessageReceived {
            message: ChatMessage {
                protocol: ProtocolId::Discord,
                conversation_id: "discord:1:2".into(),
                id: "discord:row-b".into(),
                sender: "bot".into(),
                body: "B".into(),
                outbound: true,
                delivery: Delivery::Failed,
                sent_at: 1,
                arrival: thinwire_protocol::Arrival::History,
                topic: None,
                links: Vec::new(),
                photo: None,
                audio: None,
                video: None,
                file: None,
                reactions: Vec::new(),
                reply_to: None,
                edited: false,
            },
        });
        snapshot.retry_send("discord:row-b");
        let retry = snapshot
            .take_commands()
            .into_iter()
            .find_map(|command| match command {
                AdapterCommand::ResendMessage { request, .. } => Some(request),
                _ => None,
            })
            .expect("retry");
        snapshot.apply(AdapterEvent::SendRejected {
            protocol: ProtocolId::Discord,
            conversation_id: "discord:1:2".into(),
            request: retry,
        });
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Discord,
            state: AccountState::Unlinked,
        });
        link(&mut snapshot, ProtocolId::Discord);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Discord, "discord:1:2", true),
        });
        snapshot.select_conversation("discord:1:2".into());
        assert_eq!(snapshot.compose, "A");
    }

    /// Codex P2 (PR #68): unlinking one protocol keeps the draft of the same
    /// chat id in another protocol.
    #[test]
    fn unlinking_one_protocol_keeps_another_protocols_draft() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        snapshot.extra_visible.insert(ProtocolId::Slack);
        link(&mut snapshot, ProtocolId::Slack);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Slack, "telegram:2", true),
        });
        snapshot.set_draft(
            ProtocolId::Telegram,
            "telegram:2",
            None,
            "telegram draft".into(),
        );
        snapshot.set_draft(ProtocolId::Slack, "telegram:2", None, "slack draft".into());
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Slack,
            state: AccountState::Unlinked,
        });
        snapshot.select_conversation("telegram:2".into());
        assert_eq!(snapshot.compose, "telegram draft", "no data loss");
    }

    /// A draft stays with its protocol when the user switches protocols.
    #[test]
    fn a_protocol_switch_parks_the_draft_under_the_old_protocol() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        snapshot.extra_visible.insert(ProtocolId::Slack);
        link(&mut snapshot, ProtocolId::Slack);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Slack, "slack:C1", true),
        });
        snapshot.select_protocol(ProtocolId::Telegram);
        snapshot.compose = "for telegram:1".into();
        snapshot.select_protocol(ProtocolId::Slack);
        assert!(snapshot.compose.is_empty());
        snapshot.select_protocol(ProtocolId::Telegram);
        assert_eq!(snapshot.compose, "for telegram:1");
    }

    /// Codex P2 (PR #68): the fake adapter links before its rows, so a
    /// test or an example can seed a view with it.
    #[test]
    fn the_fake_adapter_seeds_a_view() {
        use thinwire_protocol::{FakeAdapter, ProtocolAdapter};

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut fake = FakeAdapter::new();
        fake.start(tx.clone());
        fake.handle(
            AdapterCommand::Connect {
                protocol: ProtocolId::Telegram,
            },
            &tx,
        )
        .expect("connect");
        let mut snapshot = Snapshot::new();
        while let Ok(event) = rx.try_recv() {
            snapshot.apply(event);
        }
        assert_eq!(snapshot.visible_conversations().len(), 1);
        assert_eq!(snapshot.selected_messages().len(), 1);
    }

    // endregion: review fixes on #68

    // region: #70 selection and open-chat gaps with two protocols

    fn open_chats(snapshot: &mut Snapshot) -> Vec<(ProtocolId, String)> {
        snapshot
            .take_commands()
            .into_iter()
            .filter_map(|command| match command {
                AdapterCommand::OpenChat {
                    protocol,
                    conversation_id,
                } => Some((protocol, conversation_id)),
                _ => None,
            })
            .collect()
    }

    /// #70 case 1: auto-select skips a placeholder row; `OpenChat` goes only
    /// to real chats.
    #[test]
    fn auto_select_skips_a_placeholder_row() {
        let mut snapshot = shell_with(&[ProtocolId::Discord]);
        link(&mut snapshot, ProtocolId::Discord);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: discord_guild_placeholder(),
        });
        assert_eq!(snapshot.selected_conversation, None);
        assert!(
            open_chats(&mut snapshot).is_empty(),
            "no OpenChat for a placeholder"
        );
        snapshot.select_conversation("discord:guild-inbox:general".into());
        assert!(
            open_chats(&mut snapshot).is_empty(),
            "not on a click either"
        );

        snapshot.selected_conversation = None;
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Discord, "discord:real", true),
        });
        assert_eq!(
            snapshot.selected_conversation.as_deref(),
            Some("discord:real")
        );
        assert_eq!(
            open_chats(&mut snapshot),
            vec![(ProtocolId::Discord, "discord:real".to_owned())]
        );
    }

    /// #70 case 2: Telegram Ready keeps a selection in another linked protocol.
    #[test]
    fn telegram_ready_keeps_another_protocols_selection() {
        let store = SecretStore::memory();
        let mut snapshot = shell_with(&[ProtocolId::Slack]);
        link(&mut snapshot, ProtocolId::Slack);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Slack, "slack:C1", true),
        });
        assert_eq!(snapshot.selected_protocol, ProtocolId::Slack);
        complete_telegram(&mut snapshot, &store);
        assert!(snapshot.telegram_ready());
        assert_eq!(
            snapshot.selected_protocol,
            ProtocolId::Slack,
            "the user's choice stays"
        );
        assert_eq!(snapshot.selected_conversation.as_deref(), Some("slack:C1"));

        // With no session in the selected protocol, Telegram Ready selects
        // Telegram. Start on Slack, so the check needs the rule (qa L3).
        let mut fresh = shell_with(&[ProtocolId::Slack]);
        fresh.selected_protocol = ProtocolId::Slack;
        assert!(!fresh.has_session(ProtocolId::Slack));
        complete_telegram(&mut fresh, &store);
        assert_eq!(fresh.selected_protocol, ProtocolId::Telegram);
    }

    /// #70 case 3: a chat opened while Linking loads when Linked arrives.
    #[test]
    fn linked_loads_a_chat_opened_while_linking() {
        let mut snapshot = shell_with(&[ProtocolId::Slack]);
        link(&mut snapshot, ProtocolId::Slack);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Slack, "slack:C1", true),
        });
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Slack, "slack:C2", true),
        });
        snapshot.take_commands();
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Slack,
            state: AccountState::Linking,
        });
        snapshot.select_conversation("slack:C2".into());
        assert!(
            open_chats(&mut snapshot).is_empty(),
            "commands wait for Linked"
        );
        link(&mut snapshot, ProtocolId::Slack);
        assert_eq!(
            open_chats(&mut snapshot),
            vec![(ProtocolId::Slack, "slack:C2".to_owned())]
        );
    }

    /// qa L1 on #79: a normal reconnect of a chat that already loaded sends
    /// no second `OpenChat`.
    #[test]
    fn a_reconnect_does_not_reload_a_loaded_chat() {
        let mut snapshot = shell_with(&[ProtocolId::Slack]);
        link(&mut snapshot, ProtocolId::Slack);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Slack, "slack:C1", true),
        });
        assert_eq!(
            open_chats(&mut snapshot),
            vec![(ProtocolId::Slack, "slack:C1".to_owned())]
        );
        snapshot.apply(AdapterEvent::HistoryLoaded {
            protocol: ProtocolId::Slack,
            conversation_id: "slack:C1".into(),
            store_cursor: None,
        });
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Slack,
            state: AccountState::Linking,
        });
        link(&mut snapshot, ProtocolId::Slack);
        assert!(open_chats(&mut snapshot).is_empty(), "no reload");
    }

    /// #90 item 2 (Codex on #86): an `OpenChat` queued in the same pump as a
    /// later `Linking` waits for `Linked`. It is not sent during the
    /// reconnect, and `Linked` sends it.
    #[test]
    fn an_open_queued_before_linking_waits_for_linked() {
        let mut snapshot = shell_with(&[ProtocolId::Slack]);
        link(&mut snapshot, ProtocolId::Slack);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Slack, "slack:C1", true),
        });
        // The auto-select queued an OpenChat; the flush has not run yet.
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Slack,
            state: AccountState::Linking,
        });
        assert!(open_chats(&mut snapshot).is_empty(), "held for Linked");
        link(&mut snapshot, ProtocolId::Slack);
        assert_eq!(
            open_chats(&mut snapshot),
            vec![(ProtocolId::Slack, "slack:C1".to_owned())]
        );
    }

    /// #163: an older page in flight when a reconnect starts can lose its
    /// answer. Its spinner stops, and a later scroll asks again.
    #[test]
    fn a_reconnect_ends_an_older_page_in_flight() {
        let mut snapshot = shell_with(&[ProtocolId::Slack]);
        link(&mut snapshot, ProtocolId::Slack);
        let key = (ProtocolId::Slack, "slack:C1".to_owned(), None);
        snapshot.older_loading.insert(key.clone());
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Slack,
            state: AccountState::Linking,
        });
        assert!(
            !snapshot.older_loading.contains(&key),
            "the older spinner stops"
        );
        assert_ne!(
            snapshot.loading_line(ProtocolId::Slack),
            Some(LOADING_OLDER_STATUS)
        );
    }

    /// Codex r4137896062 (#187): an older-page load that is still queued
    /// when a reconnect starts does not go out. Its spinner stops too.
    #[test]
    fn a_reconnect_drops_a_queued_older_page() {
        let store = SecretStore::memory();
        let mut snapshot = chat_with_recent_page(&store);
        snapshot.load_older();
        assert_eq!(snapshot.older_state(), OlderState::Loading);
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Telegram,
            state: AccountState::Linking,
        });
        assert!(older_requests(&mut snapshot).is_empty(), "not sent");
        assert_eq!(snapshot.older_state(), OlderState::Idle);
    }

    /// #163: a chat-list load in flight when a reconnect starts can lose its
    /// answer. Its spinner stops, and `Linked` loads the list again.
    #[test]
    fn a_reconnect_ends_a_chat_list_load_and_loads_again_on_linked() {
        let mut snapshot = shell_with(&[ProtocolId::Slack]);
        link(&mut snapshot, ProtocolId::Slack);
        snapshot.take_commands();
        snapshot.refresh_visible();
        assert!(
            snapshot
                .take_commands()
                .contains(&AdapterCommand::LoadChats {
                    protocol: ProtocolId::Slack
                }),
            "the list load is sent"
        );
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Slack,
            state: AccountState::Linking,
        });
        assert!(!snapshot.chat_list_loading.contains(&ProtocolId::Slack));
        assert_eq!(snapshot.loading_line(ProtocolId::Slack), None);
        link(&mut snapshot, ProtocolId::Slack);
        assert!(snapshot.chat_list_loading.contains(&ProtocolId::Slack));
        let loads = snapshot
            .take_commands()
            .into_iter()
            .filter(|command| {
                matches!(
                    command,
                    AdapterCommand::LoadChats {
                        protocol: ProtocolId::Slack
                    }
                )
            })
            .count();
        assert_eq!(loads, 1, "Linked loads the list again, once");
    }

    /// #163: a chat-list load still queued when a reconnect starts waits for
    /// `Linked`, and goes out once then.
    #[test]
    fn a_queued_chat_list_load_waits_for_linked() {
        let mut snapshot = shell_with(&[ProtocolId::Slack]);
        link(&mut snapshot, ProtocolId::Slack);
        snapshot.take_commands();
        snapshot.refresh_visible();
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Slack,
            state: AccountState::Linking,
        });
        assert!(
            !snapshot
                .take_commands()
                .contains(&AdapterCommand::LoadChats {
                    protocol: ProtocolId::Slack
                }),
            "held for Linked"
        );
        link(&mut snapshot, ProtocolId::Slack);
        let loads = snapshot
            .take_commands()
            .into_iter()
            .filter(|command| {
                matches!(
                    command,
                    AdapterCommand::LoadChats {
                        protocol: ProtocolId::Slack
                    }
                )
            })
            .count();
        assert_eq!(loads, 1);
    }

    /// Codex r4107046484 (#124): a load of another chat that was already
    /// sent when a reconnect starts can be lost. Its spinner stops; only the
    /// selected chat loads again on Linked.
    #[test]
    fn a_reconnect_stops_the_spinner_of_a_sent_load_of_another_chat() {
        let mut snapshot = shell_with(&[ProtocolId::Slack]);
        link(&mut snapshot, ProtocolId::Slack);
        for id in ["slack:C1", "slack:C2"] {
            snapshot.apply(AdapterEvent::ConversationUpsert {
                conversation: chat(ProtocolId::Slack, id, true),
            });
        }
        snapshot.take_commands();
        snapshot.select_conversation("slack:C1".into());
        snapshot.take_commands();
        snapshot.select_conversation("slack:C2".into());
        snapshot.take_commands();
        let loading = |snapshot: &Snapshot, id: &str| {
            snapshot
                .history_loading
                .contains(&(ProtocolId::Slack, id.to_owned()))
        };
        assert!(loading(&snapshot, "slack:C1") && loading(&snapshot, "slack:C2"));
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Slack,
            state: AccountState::Linking,
        });
        assert!(
            !loading(&snapshot, "slack:C1"),
            "the other chat's spinner stops"
        );
        assert!(
            loading(&snapshot, "slack:C2"),
            "the selected chat waits for Linked"
        );
        link(&mut snapshot, ProtocolId::Slack);
        assert_eq!(
            open_chats(&mut snapshot),
            vec![(ProtocolId::Slack, "slack:C2".to_owned())]
        );
    }

    /// Codex on #111: history that comes during the reconnect clears the
    /// reopen mark, so Linked does not load the chat again.
    #[test]
    fn history_during_the_reconnect_clears_the_reopen() {
        let mut snapshot = shell_with(&[ProtocolId::Slack]);
        link(&mut snapshot, ProtocolId::Slack);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Slack, "slack:C1", true),
        });
        assert_eq!(open_chats(&mut snapshot).len(), 1, "sent, no answer yet");
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Slack,
            state: AccountState::Linking,
        });
        snapshot.apply(AdapterEvent::HistoryLoaded {
            protocol: ProtocolId::Slack,
            conversation_id: "slack:C1".into(),
            store_cursor: None,
        });
        link(&mut snapshot, ProtocolId::Slack);
        assert!(open_chats(&mut snapshot).is_empty(), "no reload");
    }

    /// qa Low on #111: a queued open of a chat that is no longer selected is
    /// dropped at Linking, and its spinner stops.
    #[test]
    fn a_dropped_open_of_another_chat_stops_its_spinner() {
        let mut snapshot = shell_with(&[ProtocolId::Slack]);
        link(&mut snapshot, ProtocolId::Slack);
        for id in ["slack:C1", "slack:C2"] {
            snapshot.apply(AdapterEvent::ConversationUpsert {
                conversation: chat(ProtocolId::Slack, id, true),
            });
        }
        snapshot.take_commands();
        snapshot.select_conversation("slack:C2".into());
        snapshot.select_conversation("slack:C1".into());
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Slack,
            state: AccountState::Linking,
        });
        assert!(
            !snapshot
                .history_loading
                .contains(&(ProtocolId::Slack, "slack:C2".to_owned())),
            "no spinner for the dropped open"
        );
        assert!(
            snapshot
                .history_loading
                .contains(&(ProtocolId::Slack, "slack:C1".to_owned())),
            "the selected chat waits for Linked"
        );
    }

    /// #90 item 7 (qa on #86): an `OpenChat` in flight when a reconnect
    /// starts can be lost. `Linked` sends it again.
    #[test]
    fn an_open_in_flight_at_linking_loads_again_on_linked() {
        let mut snapshot = shell_with(&[ProtocolId::Slack]);
        link(&mut snapshot, ProtocolId::Slack);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Slack, "slack:C1", true),
        });
        assert_eq!(open_chats(&mut snapshot).len(), 1, "sent, no answer yet");
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Slack,
            state: AccountState::Linking,
        });
        link(&mut snapshot, ProtocolId::Slack);
        assert_eq!(
            open_chats(&mut snapshot),
            vec![(ProtocolId::Slack, "slack:C1".to_owned())]
        );
    }

    /// qa L2 on #79: a placeholder row never becomes the viewed chat.
    #[test]
    fn a_placeholder_is_never_the_viewed_chat() {
        let mut snapshot = shell_with(&[ProtocolId::Discord]);
        link(&mut snapshot, ProtocolId::Discord);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: discord_guild_placeholder(),
        });
        snapshot.select_conversation("discord:guild-inbox:general".into());
        snapshot.sync_viewed();
        assert!(
            !snapshot.take_commands().iter().any(|command| matches!(
                command,
                AdapterCommand::ViewChat {
                    conversation_id: Some(_),
                    ..
                }
            )),
            "no ViewChat for a placeholder"
        );
    }

    // endregion: #70

    // region: #69 unanswered sends

    /// Sends "hello" in the selected chat and lets it expire. Returns the
    /// request id.
    fn expired_send(snapshot: &mut Snapshot) -> u64 {
        snapshot.compose = "hello".into();
        snapshot.send_compose();
        let request = snapshot
            .sends
            .request_of(ProtocolId::Telegram, "telegram:1")
            .expect("send");
        assert!(!snapshot.can_send());
        snapshot.expire_sends_at(Instant::now() + crate::sends::SEND_TIMEOUT);
        request
    }

    fn late_answer(snapshot: &mut Snapshot, request: u64, accepted: bool) {
        let conversation_id = "telegram:1".to_owned();
        let protocol = ProtocolId::Telegram;
        snapshot.apply(if accepted {
            AdapterEvent::SendAccepted {
                protocol,
                conversation_id,
                request,
            }
        } else {
            AdapterEvent::SendRejected {
                protocol,
                conversation_id,
                request,
            }
        });
    }

    /// #69: a send with no answer unlocks its chat after the timeout. The text
    /// stays and the error shows.
    #[test]
    fn an_unanswered_send_unlocks_its_chat() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        expired_send(&mut snapshot);
        assert!(snapshot.can_send(), "the chat is unlocked");
        assert_eq!(snapshot.compose, "hello", "the text stays");
        assert_eq!(
            snapshot.error.as_ref().map(|error| error.why.as_str()),
            Some("Telegram did not answer in time.")
        );
    }

    /// PR #81 review: a late accept means the message went out. The
    /// unchanged text leaves the compose field, so the user does not send it
    /// twice, and the timeout error goes.
    #[test]
    fn a_late_accept_clears_the_unchanged_text() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        let request = expired_send(&mut snapshot);
        late_answer(&mut snapshot, request, true);
        assert_eq!(snapshot.compose, "", "the sent text is cleared");
        assert!(snapshot.error.is_none(), "the timeout error goes");
    }

    /// PR #81 review: after the expiry the user edits the text. A late
    /// accept keeps the new text.
    #[test]
    fn a_late_accept_keeps_edited_text() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        let request = expired_send(&mut snapshot);
        snapshot.compose = "hello again".into();
        late_answer(&mut snapshot, request, true);
        assert_eq!(snapshot.compose, "hello again");
    }

    /// PR #81 review: a late accept for a chat the user left clears only its
    /// unchanged draft.
    #[test]
    fn a_late_accept_clears_the_unchanged_draft_of_another_chat() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        let request = expired_send(&mut snapshot);
        let key = (ProtocolId::Telegram, "telegram:1".to_owned(), None);
        snapshot.select_conversation("telegram:2".into());
        assert!(snapshot.drafts.contains_key(&key), "the text is a draft");
        late_answer(&mut snapshot, request, true);
        assert!(!snapshot.drafts.contains_key(&key));
    }

    /// PR #81 review: a late rejection after the expiry changes nothing.
    #[test]
    fn a_late_reject_changes_nothing() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        let request = expired_send(&mut snapshot);
        let error = snapshot.error.clone();
        late_answer(&mut snapshot, request, false);
        assert_eq!(snapshot.compose, "hello");
        assert_eq!(snapshot.error, error);
        assert!(snapshot.can_send());
        // A later accept for the same request finds nothing either.
        late_answer(&mut snapshot, request, true);
        assert_eq!(snapshot.compose, "hello");
    }

    /// #69: an unanswered retry sets its row back to Failed.
    #[test]
    fn an_unanswered_retry_fails_its_row_again() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        retry_failed_row(&mut snapshot);
        snapshot.expire_sends_at(Instant::now() + crate::sends::SEND_TIMEOUT);
        assert_eq!(snapshot.selected_messages()[0].delivery, Delivery::Failed);
        assert!(snapshot.error.is_some());
    }

    /// PR #81 review: a late accept of an expired retry marks its row sent.
    #[test]
    fn a_late_retry_accept_marks_the_row_sent() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        let request = retry_failed_row(&mut snapshot);
        snapshot.expire_sends_at(Instant::now() + crate::sends::SEND_TIMEOUT);
        assert_eq!(snapshot.selected_messages()[0].delivery, Delivery::Failed);
        late_answer(&mut snapshot, request, true);
        assert_eq!(snapshot.selected_messages()[0].delivery, Delivery::Sent);
        assert!(snapshot.error.is_none());
    }

    fn answer_in(snapshot: &mut Snapshot, chat: &str, request: u64, accepted: bool) {
        let conversation_id = chat.to_owned();
        let protocol = ProtocolId::Telegram;
        snapshot.apply(if accepted {
            AdapterEvent::SendAccepted {
                protocol,
                conversation_id,
                request,
            }
        } else {
            AdapterEvent::SendRejected {
                protocol,
                conversation_id,
                request,
            }
        });
    }

    /// Codex on #81: a late accept of a timed-out retry wins over a newer
    /// retry of the same row. The newer retry's rejection cannot fail the
    /// sent row again, and the chat unlocks.
    #[test]
    fn a_late_retry_accept_untracks_the_newer_retry() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        let first = retry_failed_row(&mut snapshot);
        snapshot.expire_sends_at(Instant::now() + crate::sends::SEND_TIMEOUT);
        let second = retry_failed_row_again(&mut snapshot);
        assert_ne!(first, second);

        answer_in(&mut snapshot, "telegram:1", first, true);
        assert_eq!(snapshot.selected_messages()[0].delivery, Delivery::Sent);
        assert_eq!(
            snapshot
                .sends
                .request_of(ProtocolId::Telegram, "telegram:1"),
            None,
            "the newer retry is no longer tracked"
        );
        answer_in(&mut snapshot, "telegram:1", second, false);
        assert_eq!(snapshot.selected_messages()[0].delivery, Delivery::Sent);
        assert!(snapshot.error.is_none(), "no error for the old retry");
    }

    /// Retries the row that `retry_failed_row` made, after it failed again.
    fn retry_failed_row_again(snapshot: &mut Snapshot) -> u64 {
        assert_eq!(snapshot.selected_messages()[0].delivery, Delivery::Failed);
        snapshot.error = None;
        snapshot.retry_send("telegram:1:101");
        snapshot
            .take_commands()
            .iter()
            .find_map(|command| match command {
                AdapterCommand::ResendMessage { request, .. } => Some(*request),
                _ => None,
            })
            .expect("a second ResendMessage")
    }

    /// Two sends in two chats, both expired in one pump. Returns the two
    /// request ids: Ada's (telegram:1) and Bob's (telegram:2, selected).
    fn two_expired_sends(snapshot: &mut Snapshot) -> (u64, u64) {
        snapshot.compose = "to ada".into();
        snapshot.send_compose();
        let ada = snapshot
            .sends
            .request_of(ProtocolId::Telegram, "telegram:1")
            .expect("send to Ada");
        snapshot.select_conversation("telegram:2".into());
        snapshot.compose = "to bob".into();
        snapshot.send_compose();
        let bob = snapshot
            .sends
            .request_of(ProtocolId::Telegram, "telegram:2")
            .expect("send to Bob");
        snapshot.expire_sends_at(Instant::now() + crate::sends::SEND_TIMEOUT);
        (ada, bob)
    }

    /// qa L2 on #81: two expiries in one pump both show, by chat.
    #[test]
    fn two_expired_sends_both_show() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        two_expired_sends(&mut snapshot);
        let error = snapshot.error.clone().expect("an error");
        assert_eq!(error.happened, "2 messages not sent.");
        assert!(error.why.contains("Ada (Telegram)"), "{error:?}");
        assert!(error.why.contains("Bob (Telegram)"), "{error:?}");
    }

    /// Codex on #81: a late accept removes only its own send from the
    /// timeout error. The other chat's send still shows.
    #[test]
    fn a_late_accept_clears_only_its_own_timeout() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        let (ada, _bob) = two_expired_sends(&mut snapshot);
        answer_in(&mut snapshot, "telegram:1", ada, true);
        assert_eq!(
            snapshot.error,
            Some(UserError {
                happened: "Message not sent.".into(),
                why: "Telegram did not answer in time.".into(),
                next: "The text is still in the compose field. Send it again.".into(),
            }),
            "Bob's send still shows"
        );
    }

    /// Codex on #81: a late accept never clears an error that is not the
    /// timeout error.
    #[test]
    fn a_late_accept_keeps_another_error() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        let request = expired_send(&mut snapshot);
        snapshot.set_error(
            "Chat not loaded.",
            "Telegram did not answer in time.",
            "Try again.",
        );
        late_answer(&mut snapshot, request, true);
        assert_eq!(
            snapshot.error.as_ref().map(|error| error.happened.as_str()),
            Some("Chat not loaded."),
            "the same why text, but not the timeout error"
        );
    }

    /// qa L1 on #81: when the user left the chat, the error names its draft.
    #[test]
    fn a_timeout_in_another_chat_names_its_draft() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        snapshot.compose = "hello".into();
        snapshot.send_compose();
        snapshot.select_conversation("telegram:2".into());
        snapshot.expire_sends_at(Instant::now() + crate::sends::SEND_TIMEOUT);
        assert_eq!(
            snapshot.error.as_ref().map(|error| error.next.as_str()),
            Some("The text is in the draft of Ada. Send it again.")
        );
    }

    /// #90 item 4: a session end removes its timed-out sends from the
    /// timeout error. A later timeout of another protocol does not list them.
    #[test]
    fn a_session_end_drops_its_timed_out_sends() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        snapshot.extra_visible.insert(ProtocolId::Slack);
        link(&mut snapshot, ProtocolId::Slack);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Slack, "slack:C1", true),
        });
        snapshot.select_protocol(ProtocolId::Telegram);
        snapshot.select_conversation("telegram:1".into());
        snapshot.compose = "to ada".into();
        snapshot.send_compose();
        snapshot.expire_sends_at(Instant::now() + crate::sends::SEND_TIMEOUT);
        assert!(snapshot.error.is_some());

        snapshot.select_protocol(ProtocolId::Slack);
        snapshot.select_conversation("slack:C1".into());
        snapshot.compose = "to slack".into();
        snapshot.send_compose();

        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Telegram,
            state: AccountState::Unlinked,
        });
        assert!(snapshot.error.is_none(), "Telegram's timeout left with it");

        snapshot.expire_sends_at(Instant::now() + crate::sends::SEND_TIMEOUT);
        let error = snapshot.error.clone().expect("Slack's timeout");
        assert_eq!(error.why, "Slack did not answer in time.");
    }

    /// #90 item 6: the timeout hint follows a chat switch.
    #[test]
    fn the_timeout_hint_follows_a_chat_switch() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        expired_send(&mut snapshot);
        let next = |snapshot: &Snapshot| snapshot.error.as_ref().map(|error| error.next.clone());
        assert_eq!(
            next(&snapshot).as_deref(),
            Some("The text is still in the compose field. Send it again.")
        );
        snapshot.select_conversation("telegram:2".into());
        snapshot.sync_timeout_hint();
        assert_eq!(
            next(&snapshot).as_deref(),
            Some("The text is in the draft of Ada. Send it again.")
        );
        snapshot.select_conversation("telegram:1".into());
        snapshot.sync_timeout_hint();
        assert_eq!(
            next(&snapshot).as_deref(),
            Some("The text is still in the compose field. Send it again.")
        );

        // Another error stays as it is.
        snapshot.set_error("Chat not loaded.", "Why.", "Next.");
        snapshot.select_conversation("telegram:2".into());
        snapshot.sync_timeout_hint();
        assert_eq!(next(&snapshot).as_deref(), Some("Next."));
    }

    /// #90 item 5: the line shows "Sending…" only while a send is in flight.
    /// After the last send times out, the line is no longer busy.
    #[test]
    fn the_sending_line_ends_with_the_last_timeout() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        snapshot.apply(AdapterEvent::ChatListLoaded {
            protocol: ProtocolId::Telegram,
        });
        snapshot.history_loading.clear();
        snapshot.status_text = "Telegram is ready.".into();
        snapshot.compose = "hello".into();
        snapshot.send_compose();
        assert_eq!(snapshot.status_line(), SENDING_STATUS);
        snapshot.expire_sends_at(Instant::now() + crate::sends::SEND_TIMEOUT);
        assert_eq!(snapshot.status_line(), "Telegram is ready.");
        assert!(!snapshot.status_line_loads());
    }

    // endregion: #69

    // region: #80 one status line per protocol

    /// #80: a Telegram older page ends while Slack still loads. The strip
    /// shows Slack's loading line, never the finished "Message sent.".
    #[test]
    fn a_running_load_line_wins_over_a_finished_ready_line() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        snapshot.extra_visible.insert(ProtocolId::Slack);
        snapshot.apply(AdapterEvent::ChatListLoaded {
            protocol: ProtocolId::Telegram,
        });
        snapshot.history_loading.clear();
        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Telegram,
            status: AdapterStatus::Ready,
            detail: "Message sent.".into(),
        });
        assert!(snapshot.status_is_idle());
        assert_eq!(snapshot.status_line(), "Message sent.");

        link(&mut snapshot, ProtocolId::Slack);
        snapshot.chat_list_loading.insert(ProtocolId::Slack);
        snapshot
            .older_loading
            .insert((ProtocolId::Telegram, "telegram:1".into(), None));
        assert_eq!(
            snapshot.status_line(),
            LOADING_OLDER_STATUS,
            "selected first"
        );

        // The Telegram older page ends; Slack still loads its chat list.
        snapshot.older_loading.clear();
        assert!(snapshot.is_loading());
        assert!(!snapshot.status_is_idle());
        assert_eq!(snapshot.status_line(), "Slack: Loading chats…");

        snapshot.apply(AdapterEvent::ChatListLoaded {
            protocol: ProtocolId::Slack,
        });
        assert_eq!(snapshot.status_line(), "Message sent.", "idle again");
    }

    /// #80: each protocol has its own loading line; the selected one wins.
    #[test]
    fn each_protocol_has_its_own_loading_line() {
        let mut snapshot = shell_with(&[ProtocolId::Slack, ProtocolId::Discord]);
        link(&mut snapshot, ProtocolId::Slack);
        link(&mut snapshot, ProtocolId::Discord);
        allow_send(&mut snapshot, ProtocolId::Slack);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Slack, "slack:C1", true),
        });
        snapshot.chat_list_loading.insert(ProtocolId::Discord);
        snapshot.history_loading.clear();
        snapshot.select_protocol(ProtocolId::Slack);
        snapshot.history_loading.clear();
        snapshot.compose = "hi".into();
        snapshot.send_compose();
        assert_eq!(
            snapshot.loading_line(ProtocolId::Slack),
            Some(SENDING_STATUS)
        );
        assert_eq!(
            snapshot.loading_line(ProtocolId::Discord),
            Some(LOADING_CHATS_STATUS)
        );
        assert_eq!(
            snapshot.status_line(),
            SENDING_STATUS,
            "the selected protocol first"
        );
        snapshot.select_protocol(ProtocolId::Discord);
        assert_eq!(snapshot.status_line(), LOADING_CHATS_STATUS);
    }

    /// A Telegram session with its loads done, and Slack linked and visible.
    fn telegram_idle_slack_linked(store: &SecretStore) -> Snapshot {
        let mut snapshot = ready_with_chats(store);
        snapshot.extra_visible.insert(ProtocolId::Slack);
        snapshot.apply(AdapterEvent::ChatListLoaded {
            protocol: ProtocolId::Telegram,
        });
        snapshot.history_loading.clear();
        link(&mut snapshot, ProtocolId::Slack);
        snapshot
    }

    /// #90 item 1: a Telegram failure line wins over a load of another
    /// protocol. After a later Ready line, the load shows again.
    #[test]
    fn a_telegram_failure_wins_over_another_protocols_load() {
        let store = SecretStore::memory();
        let mut snapshot = telegram_idle_slack_linked(&store);
        snapshot.chat_list_loading.insert(ProtocolId::Slack);
        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Telegram,
            status: AdapterStatus::Error,
            detail: "Telegram connection lost.".into(),
        });
        assert_eq!(snapshot.status_line(), "Telegram connection lost.");
        assert!(!snapshot.status_line_loads(), "a failure is not busy");

        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Telegram,
            status: AdapterStatus::Ready,
            detail: "Message sent.".into(),
        });
        assert_eq!(snapshot.status_line(), "Slack: Loading chats…");
        assert!(snapshot.status_line_loads());
    }

    /// #124: the user looks at Slack while a Slack chat loads and a Telegram
    /// failure is the last status. The strip shows the load of the chat on
    /// screen first. The Telegram failure shows when that load ends.
    #[test]
    fn the_selected_protocols_load_shows_before_a_telegram_failure() {
        let store = SecretStore::memory();
        let mut snapshot = telegram_idle_slack_linked(&store);
        snapshot.select_protocol(ProtocolId::Slack);
        snapshot
            .history_loading
            .insert((ProtocolId::Slack, "slack:C1".to_owned()));
        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Telegram,
            status: AdapterStatus::Error,
            detail: "Telegram connection lost.".into(),
        });
        assert_eq!(snapshot.status_line(), LOADING_MESSAGES_STATUS);
        assert!(snapshot.status_line_loads());

        snapshot.apply(AdapterEvent::HistoryLoaded {
            protocol: ProtocolId::Slack,
            conversation_id: "slack:C1".into(),
            store_cursor: None,
        });
        assert_eq!(snapshot.status_line(), "Telegram connection lost.");
        assert!(!snapshot.status_line_loads(), "a failure is not busy");
    }

    /// #82 qa L1: a load of a protocol with no visible surface does not make
    /// the shown line busy.
    #[test]
    fn a_hidden_protocols_load_does_not_make_the_line_busy() {
        let store = SecretStore::memory();
        let mut snapshot = telegram_idle_slack_linked(&store);
        snapshot.extra_visible.remove(&ProtocolId::Slack);
        // Hidden in every feature build, also with slack-oauth on.
        snapshot.hidden_for_test.insert(ProtocolId::Slack);
        snapshot.chat_list_loading.insert(ProtocolId::Slack);
        snapshot.status_text = "Message sent.".into();
        assert!(snapshot.is_loading());
        assert_eq!(snapshot.status_line(), "Message sent.");
        assert!(!snapshot.status_line_loads());
    }

    // endregion: #80

    // region: #135 pending sends at a session end

    /// A Slack send in flight in the selected chat. Returns its request.
    fn slack_send_in_flight(snapshot: &mut Snapshot, text: &str) -> u64 {
        link(snapshot, ProtocolId::Slack);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Slack, "slack:C1", true),
        });
        snapshot.select_conversation("slack:C1".into());
        snapshot.compose = text.into();
        snapshot.send_compose();
        snapshot
            .sends
            .request_of(ProtocolId::Slack, "slack:C1")
            .expect("send in flight")
    }

    /// #135: a session end keeps the text of a send in flight as its chat's
    /// draft. A late SendRejected does not clear it. After a relink the text
    /// is back in the compose field.
    #[test]
    fn a_session_end_keeps_the_text_of_a_send_in_flight() {
        let mut snapshot = shell_with(&[ProtocolId::Slack]);
        let request = slack_send_in_flight(&mut snapshot, "hello");
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Slack,
            state: AccountState::Unlinked,
        });
        assert_eq!(
            snapshot
                .drafts
                .get(&(ProtocolId::Slack, "slack:C1".to_owned(), None))
                .map(String::as_str),
            Some("hello")
        );

        link(&mut snapshot, ProtocolId::Slack);
        snapshot.apply(AdapterEvent::SendRejected {
            protocol: ProtocolId::Slack,
            conversation_id: "slack:C1".into(),
            request,
        });
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Slack, "slack:C1", true),
        });
        snapshot.select_conversation("slack:C1".into());
        assert_eq!(snapshot.compose, "hello", "the text is back");
    }

    /// #135: a session end keeps the pending text only when the chat's text
    /// is empty or still that text. Other typed text is not overwritten.
    #[test]
    fn a_session_end_does_not_overwrite_other_text_with_a_pending_send() {
        let mut snapshot = shell_with(&[ProtocolId::Slack]);
        slack_send_in_flight(&mut snapshot, "hello");
        snapshot.compose = "something else".into();
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Slack,
            state: AccountState::Unlinked,
        });
        assert_eq!(
            snapshot
                .drafts
                .get(&(ProtocolId::Slack, "slack:C1".to_owned(), None)),
            None
        );
    }

    /// Codex r4109058384 on #135: a send that timed out keeps its text too.
    /// Send, timeout, Unlinked, relink: the text is back.
    #[test]
    fn a_session_end_keeps_the_text_of_an_expired_send() {
        let mut snapshot = shell_with(&[ProtocolId::Slack]);
        slack_send_in_flight(&mut snapshot, "hello");
        snapshot.expire_sends_at(Instant::now() + crate::sends::SEND_TIMEOUT);
        assert!(snapshot.can_send(), "the send expired");
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Slack,
            state: AccountState::Unlinked,
        });
        assert_eq!(
            snapshot
                .drafts
                .get(&(ProtocolId::Slack, "slack:C1".to_owned(), None))
                .map(String::as_str),
            Some("hello")
        );
        link(&mut snapshot, ProtocolId::Slack);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Slack, "slack:C1", true),
        });
        snapshot.select_conversation("slack:C1".into());
        assert_eq!(snapshot.compose, "hello", "the text is back");
    }

    /// #135 keeps the R73 rule: a Telegram session end drops every draft,
    /// also the text of a send in flight.
    #[test]
    fn a_telegram_session_end_still_drops_a_pending_send_text() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        snapshot.compose = "hello".into();
        snapshot.send_compose();
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Telegram,
            state: AccountState::Unlinked,
        });
        assert!(
            !snapshot
                .drafts
                .keys()
                .any(|(owner, _, _)| *owner == ProtocolId::Telegram)
        );
        assert_eq!(snapshot.compose, "");
    }

    /// qa Low on #135 (#138): a retry in flight when the session ends
    /// keeps the text of its row as the chat's draft. The session end drops
    /// the row itself.
    #[test]
    fn a_session_end_keeps_the_text_of_a_retry_in_flight() {
        let mut snapshot = shell_with(&[ProtocolId::Slack]);
        link(&mut snapshot, ProtocolId::Slack);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: chat(ProtocolId::Slack, "slack:C1", true),
        });
        snapshot.select_conversation("slack:C1".into());
        snapshot.apply(AdapterEvent::MessageReceived {
            message: ChatMessage {
                protocol: ProtocolId::Slack,
                conversation_id: "slack:C1".into(),
                id: "slack:C1:m1".into(),
                sender: "You".into(),
                body: "try again".into(),
                outbound: true,
                delivery: Delivery::Failed,
                sent_at: 1,
                arrival: thinwire_protocol::Arrival::History,
                topic: None,
                links: Vec::new(),
                photo: None,
                audio: None,
                video: None,
                file: None,
                reactions: Vec::new(),
                reply_to: None,
                edited: false,
            },
        });
        snapshot.retry_send("slack:C1:m1");
        assert!(
            snapshot
                .sends
                .request_of(ProtocolId::Slack, "slack:C1")
                .is_some(),
            "the retry is in flight"
        );
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Slack,
            state: AccountState::Unlinked,
        });
        assert_eq!(
            snapshot
                .drafts
                .get(&(ProtocolId::Slack, "slack:C1".to_owned(), None))
                .map(String::as_str),
            Some("try again")
        );
    }

    /// #174 item 7: the default shell accepts `Unlinked` for a Discord
    /// account that was never linked. The account stays idle.
    #[test]
    fn an_unlinked_discord_account_that_was_never_linked_stays_idle() {
        let mut snapshot = Snapshot::new();
        let selected = snapshot.selected_protocol;
        let discord = snapshot
            .accounts
            .iter()
            .find(|row| row.caps.id == ProtocolId::Discord)
            .expect("discord is in the catalog");
        assert_eq!(discord.state, AccountState::Unlinked);
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Discord,
            state: AccountState::Unlinked,
        });
        assert!(!snapshot.protocol_linked(ProtocolId::Discord));
        assert_eq!(snapshot.selected_protocol, selected);
        assert!(snapshot.error.is_none());
        let discord = snapshot
            .accounts
            .iter()
            .find(|row| row.caps.id == ProtocolId::Discord)
            .expect("discord stays in the catalog");
        assert_eq!(discord.state, AccountState::Unlinked);
    }

    fn signal_chat(id: &str) -> Conversation {
        Conversation {
            protocol: ProtocolId::Signal,
            id: id.into(),
            title: "Ada".into(),
            participant: "Ada".into(),
            preview: String::new(),
            unread: 0,
            order: 1,
            last_at: 0,
            is_group: false,
            writable: true,
            placeholder: false,
            muted: false,
            has_topics: false,
            avatar: Default::default(),
        }
    }

    fn signal_row(chat: &str, millis: i64, body: &str) -> ChatMessage {
        ChatMessage {
            protocol: ProtocolId::Signal,
            conversation_id: chat.into(),
            id: format!("{millis}:ada"),
            sender: "Ada".into(),
            body: body.into(),
            outbound: false,
            delivery: Delivery::Sent,
            sent_at: millis,
            arrival: thinwire_protocol::Arrival::History,
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

    fn linked_signal_chat() -> (Snapshot, &'static str) {
        let mut snapshot = Snapshot::new();
        show_protocol(&mut snapshot, ProtocolId::Signal);
        show_only(&mut snapshot, &[ProtocolId::Signal]);
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Signal,
            state: AccountState::Linked,
        });
        let chat = "signal-chat";
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: signal_chat(chat),
        });
        snapshot.select_conversation(chat.to_string());
        snapshot.take_commands();
        (snapshot, chat)
    }

    fn older_store_cursors(snapshot: &mut Snapshot) -> Vec<Option<u64>> {
        snapshot
            .take_commands()
            .into_iter()
            .filter_map(|command| match command {
                AdapterCommand::LoadOlderMessages { store_cursor, .. } => Some(store_cursor),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn unreadable_lines_name_the_count_without_a_fault_word() {
        assert_eq!(
            unreadable_messages_line(0),
            "Some messages here could not be read. They were not deleted."
        );
        assert_eq!(
            unreadable_messages_line(1),
            "1 message here could not be read. It was not deleted."
        );
        assert_eq!(
            unreadable_messages_line(300),
            "300 messages here could not be read. They were not deleted."
        );
        for count in [0, 1, 2, 300] {
            let line = unreadable_messages_line(count).to_lowercase();
            assert!(!line.contains("corrupt"), "{line}");
            assert!(!line.contains("error"), "{line}");
            assert!(!line.contains("failed"), "{line}");
        }
    }

    #[test]
    fn three_hundred_unreadable_rows_show_one_marker_and_the_good_rows() {
        let (mut snapshot, chat) = linked_signal_chat();
        snapshot.apply(AdapterEvent::HistoryLoaded {
            protocol: ProtocolId::Signal,
            conversation_id: chat.into(),
            store_cursor: Some(HistoryStoreCursor {
                before_ms: 5_000,
                skipped: 208,
                resume: true,
                runs: Vec::new(),
            }),
        });
        assert_eq!(snapshot.older_state(), OlderState::Loading);
        assert!(snapshot.unreadable_runs().is_empty(), "the run stays open");
        assert_eq!(older_store_cursors(&mut snapshot), vec![Some(5_000)]);
        for n in 1..=5 {
            snapshot.apply(AdapterEvent::MessageReceived {
                message: signal_row(chat, 1_000 + n, &format!("good-{n}")),
            });
        }
        snapshot.apply(AdapterEvent::OlderHistoryLoaded {
            protocol: ProtocolId::Signal,
            conversation_id: chat.into(),
            before_message_id: "5000".into(),
            more: true,
            note: None,
            topic: None,
            store_cursor: Some(HistoryStoreCursor {
                before_ms: 1_000,
                skipped: 92,
                resume: false,
                runs: Vec::new(),
            }),
        });
        let runs = snapshot.unreadable_runs();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].skipped, 300);
        assert_eq!(runs[0].before_id, None, "the run is the newest content");
        assert_eq!(snapshot.selected_messages().len(), 5);
        assert_eq!(snapshot.selected_messages()[0].body, "good-1");
        assert_eq!(snapshot.older_state(), OlderState::Idle);
        assert!(older_store_cursors(&mut snapshot).is_empty());
    }

    #[test]
    fn an_empty_page_of_208_unreadable_rows_loads_the_good_rows() {
        let (mut snapshot, chat) = linked_signal_chat();
        snapshot.apply(AdapterEvent::HistoryLoaded {
            protocol: ProtocolId::Signal,
            conversation_id: chat.into(),
            store_cursor: Some(HistoryStoreCursor {
                before_ms: 4_000,
                skipped: 208,
                resume: true,
                runs: Vec::new(),
            }),
        });
        assert_eq!(older_store_cursors(&mut snapshot), vec![Some(4_000)]);
        snapshot.apply(AdapterEvent::MessageReceived {
            message: signal_row(chat, 100, "kept"),
        });
        snapshot.apply(AdapterEvent::OlderHistoryLoaded {
            protocol: ProtocolId::Signal,
            conversation_id: chat.into(),
            before_message_id: "4000".into(),
            more: false,
            note: None,
            topic: None,
            store_cursor: Some(HistoryStoreCursor {
                before_ms: 50,
                skipped: 0,
                resume: false,
                runs: Vec::new(),
            }),
        });
        assert_eq!(snapshot.unreadable_runs().len(), 1);
        assert_eq!(snapshot.unreadable_runs()[0].skipped, 208);
        assert_eq!(snapshot.selected_messages()[0].body, "kept");
        assert_eq!(snapshot.older_state(), OlderState::StartOfChat);
    }

    #[test]
    fn an_all_skipped_page_loads_the_next_page_without_a_marker() {
        let (mut snapshot, chat) = linked_signal_chat();
        snapshot.apply(AdapterEvent::MessageReceived {
            message: signal_row(chat, 9_000, "on screen"),
        });
        snapshot.apply(AdapterEvent::HistoryLoaded {
            protocol: ProtocolId::Signal,
            conversation_id: chat.into(),
            store_cursor: None,
        });
        snapshot.take_commands();
        snapshot.load_older();
        assert_eq!(older_store_cursors(&mut snapshot), vec![None]);
        snapshot.apply(AdapterEvent::OlderHistoryLoaded {
            protocol: ProtocolId::Signal,
            conversation_id: chat.into(),
            before_message_id: "9000:ada".into(),
            more: true,
            note: None,
            topic: None,
            store_cursor: Some(HistoryStoreCursor {
                before_ms: 3_000,
                skipped: 208,
                resume: true,
                runs: Vec::new(),
            }),
        });
        assert!(snapshot.unreadable_runs().is_empty());
        assert_eq!(snapshot.older_state(), OlderState::Loading);
        assert_eq!(older_store_cursors(&mut snapshot), vec![Some(3_000)]);
        for n in 1..=3 {
            snapshot.apply(AdapterEvent::MessageReceived {
                message: signal_row(chat, n, &format!("old-{n}")),
            });
        }
        snapshot.apply(AdapterEvent::OlderHistoryLoaded {
            protocol: ProtocolId::Signal,
            conversation_id: chat.into(),
            before_message_id: "9000:ada".into(),
            more: true,
            note: None,
            topic: None,
            store_cursor: Some(HistoryStoreCursor {
                before_ms: 1,
                skipped: 92,
                resume: false,
                runs: Vec::new(),
            }),
        });
        let runs = snapshot.unreadable_runs();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].skipped, 300);
        assert_eq!(runs[0].before_id.as_deref(), Some("9000:ada"));
        assert_eq!(snapshot.older_state(), OlderState::Idle);
        assert_eq!(snapshot.selected_messages().len(), 4);
    }

    #[test]
    fn one_unreadable_row_uses_the_singular_line() {
        let (mut snapshot, chat) = linked_signal_chat();
        snapshot.apply(AdapterEvent::MessageReceived {
            message: signal_row(chat, 5_000, "newer"),
        });
        snapshot.apply(AdapterEvent::HistoryLoaded {
            protocol: ProtocolId::Signal,
            conversation_id: chat.into(),
            store_cursor: None,
        });
        snapshot.take_commands();
        snapshot.load_older();
        snapshot.take_commands();
        snapshot.apply(AdapterEvent::MessageReceived {
            message: signal_row(chat, 4_000, "older"),
        });
        snapshot.apply(AdapterEvent::OlderHistoryLoaded {
            protocol: ProtocolId::Signal,
            conversation_id: chat.into(),
            before_message_id: "5000:ada".into(),
            more: false,
            note: None,
            topic: None,
            store_cursor: Some(HistoryStoreCursor {
                before_ms: 3_000,
                skipped: 1,
                resume: false,
                runs: Vec::new(),
            }),
        });
        assert_eq!(snapshot.unreadable_runs()[0].skipped, 1);
        assert_eq!(
            snapshot.unreadable_runs()[0].before_id.as_deref(),
            Some("5000:ada")
        );
        assert_eq!(
            unreadable_messages_line(1),
            "1 message here could not be read. It was not deleted."
        );
        assert_eq!(snapshot.older_state(), OlderState::StartOfChat);
    }

    #[test]
    fn a_store_cursor_that_does_not_move_is_not_asked_again() {
        let (mut snapshot, chat) = linked_signal_chat();
        snapshot.apply(AdapterEvent::HistoryLoaded {
            protocol: ProtocolId::Signal,
            conversation_id: chat.into(),
            store_cursor: Some(HistoryStoreCursor {
                before_ms: 100,
                skipped: 10,
                resume: true,
                runs: Vec::new(),
            }),
        });
        assert_eq!(older_store_cursors(&mut snapshot), vec![Some(100)]);
        snapshot.apply(AdapterEvent::OlderHistoryLoaded {
            protocol: ProtocolId::Signal,
            conversation_id: chat.into(),
            before_message_id: "100".into(),
            more: true,
            note: None,
            topic: None,
            store_cursor: Some(HistoryStoreCursor {
                before_ms: 100,
                skipped: 10,
                resume: true,
                runs: Vec::new(),
            }),
        });
        assert!(older_store_cursors(&mut snapshot).is_empty());
        assert_eq!(snapshot.unreadable_runs()[0].skipped, 20);
        assert_ne!(snapshot.older_state(), OlderState::Loading);
    }

    #[test]
    fn a_failed_store_cursor_continuation_is_asked_again() {
        let (mut snapshot, chat) = linked_signal_chat();
        snapshot.apply(AdapterEvent::MessageReceived {
            message: signal_row(chat, 9_000, "on screen"),
        });
        snapshot.apply(AdapterEvent::HistoryLoaded {
            protocol: ProtocolId::Signal,
            conversation_id: chat.into(),
            store_cursor: None,
        });
        snapshot.take_commands();
        snapshot.load_older();
        assert_eq!(older_store_cursors(&mut snapshot), vec![None]);
        snapshot.apply(AdapterEvent::OlderHistoryLoaded {
            protocol: ProtocolId::Signal,
            conversation_id: chat.into(),
            before_message_id: "9000:ada".into(),
            more: true,
            note: None,
            topic: None,
            store_cursor: Some(HistoryStoreCursor {
                before_ms: 3_000,
                skipped: 208,
                resume: true,
                runs: Vec::new(),
            }),
        });
        assert_eq!(older_store_cursors(&mut snapshot), vec![Some(3_000)]);
        snapshot.apply(AdapterEvent::OlderHistoryLoaded {
            protocol: ProtocolId::Signal,
            conversation_id: chat.into(),
            before_message_id: "9000:ada".into(),
            more: true,
            note: Some("the store read failed".into()),
            topic: None,
            store_cursor: None,
        });
        assert!(
            snapshot.unreadable_runs().is_empty(),
            "a failed continuation does not close the run"
        );
        snapshot.load_older_at(Instant::now() + OLDER_RETRY_DELAY + Duration::from_secs(1));
        assert_eq!(
            older_store_cursors(&mut snapshot),
            vec![Some(3_000)],
            "the retry sends the same store cursor"
        );
    }

    #[test]
    fn unreadable_runs_sit_between_the_readable_rows() {
        let (mut snapshot, chat) = linked_signal_chat();
        snapshot.apply(AdapterEvent::MessageReceived {
            message: signal_row(chat, 9_000, "anchor"),
        });
        snapshot.apply(AdapterEvent::HistoryLoaded {
            protocol: ProtocolId::Signal,
            conversation_id: chat.into(),
            store_cursor: None,
        });
        snapshot.take_commands();
        for (millis, body) in [(8_000, "newer"), (6_000, "middle"), (4_000, "older")] {
            snapshot.apply(AdapterEvent::MessageReceived {
                message: signal_row(chat, millis, body),
            });
        }
        snapshot.apply(AdapterEvent::OlderHistoryLoaded {
            protocol: ProtocolId::Signal,
            conversation_id: chat.into(),
            before_message_id: "9000:ada".into(),
            more: false,
            note: None,
            topic: None,
            store_cursor: Some(HistoryStoreCursor {
                before_ms: 3_000,
                skipped: 3,
                resume: false,
                runs: vec![
                    thinwire_protocol::HistorySkipRun {
                        after_id: Some("8000:ada".into()),
                        skipped: 1,
                    },
                    thinwire_protocol::HistorySkipRun {
                        after_id: Some("6000:ada".into()),
                        skipped: 2,
                    },
                ],
            }),
        });
        let runs = snapshot.unreadable_runs();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].before_id.as_deref(), Some("8000:ada"));
        assert_eq!(runs[0].skipped, 1);
        assert_eq!(runs[1].before_id.as_deref(), Some("6000:ada"));
        assert_eq!(runs[1].skipped, 2);
        assert!(
            runs.iter()
                .all(|run| run.before_id.as_deref() != Some("9000:ada")),
            "the runs do not collapse onto the request anchor"
        );
        assert_eq!(snapshot.older_state(), OlderState::StartOfChat);
    }

    #[test]
    fn an_oldest_unreadable_run_stays_when_the_store_ends() {
        let (mut snapshot, chat) = linked_signal_chat();
        snapshot.apply(AdapterEvent::MessageReceived {
            message: signal_row(chat, 8_000, "oldest readable"),
        });
        snapshot.apply(AdapterEvent::HistoryLoaded {
            protocol: ProtocolId::Signal,
            conversation_id: chat.into(),
            store_cursor: None,
        });
        snapshot.take_commands();
        snapshot.apply(AdapterEvent::OlderHistoryLoaded {
            protocol: ProtocolId::Signal,
            conversation_id: chat.into(),
            before_message_id: "8000:ada".into(),
            more: false,
            note: None,
            topic: None,
            store_cursor: Some(HistoryStoreCursor {
                before_ms: 1_000,
                skipped: 3,
                resume: false,
                runs: vec![thinwire_protocol::HistorySkipRun {
                    after_id: Some("8000:ada".into()),
                    skipped: 3,
                }],
            }),
        });
        let runs = snapshot.unreadable_runs();
        assert_eq!(runs.len(), 1, "the oldest run is still drawn");
        assert_eq!(runs[0].skipped, 3);
        assert_eq!(runs[0].before_id.as_deref(), Some("8000:ada"));
        assert_eq!(snapshot.older_state(), OlderState::StartOfChat);
    }

    #[test]
    fn a_run_across_a_page_edge_is_one_line() {
        let (mut snapshot, chat) = linked_signal_chat();
        snapshot.apply(AdapterEvent::MessageReceived {
            message: signal_row(chat, 9_000, "anchor"),
        });
        snapshot.apply(AdapterEvent::HistoryLoaded {
            protocol: ProtocolId::Signal,
            conversation_id: chat.into(),
            store_cursor: None,
        });
        snapshot.take_commands();
        snapshot.apply(AdapterEvent::MessageReceived {
            message: signal_row(chat, 8_000, "page edge"),
        });
        snapshot.apply(AdapterEvent::OlderHistoryLoaded {
            protocol: ProtocolId::Signal,
            conversation_id: chat.into(),
            before_message_id: "9000:ada".into(),
            more: true,
            note: None,
            topic: None,
            store_cursor: None,
        });
        assert!(
            snapshot.unreadable_runs().is_empty(),
            "the trailing part of the run is not drawn on this page"
        );
        snapshot.apply(AdapterEvent::MessageReceived {
            message: signal_row(chat, 5_000, "older"),
        });
        snapshot.apply(AdapterEvent::OlderHistoryLoaded {
            protocol: ProtocolId::Signal,
            conversation_id: chat.into(),
            before_message_id: "8000:ada".into(),
            more: false,
            note: None,
            topic: None,
            store_cursor: Some(HistoryStoreCursor {
                before_ms: 4_000,
                skipped: 4,
                resume: false,
                runs: vec![thinwire_protocol::HistorySkipRun {
                    after_id: None,
                    skipped: 4,
                }],
            }),
        });
        let runs = snapshot.unreadable_runs();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].skipped, 4, "the whole run is counted once");
        assert_eq!(runs[0].before_id.as_deref(), Some("8000:ada"));
        assert_eq!(snapshot.older_state(), OlderState::StartOfChat);
    }

    #[test]
    fn a_run_for_an_unknown_message_joins_the_oldest_line() {
        let (mut snapshot, chat) = linked_signal_chat();
        snapshot.apply(AdapterEvent::MessageReceived {
            message: signal_row(chat, 8_000, "newer"),
        });
        snapshot.apply(AdapterEvent::MessageReceived {
            message: signal_row(chat, 6_000, "oldest"),
        });
        snapshot.apply(AdapterEvent::HistoryLoaded {
            protocol: ProtocolId::Signal,
            conversation_id: chat.into(),
            store_cursor: None,
        });
        snapshot.take_commands();
        snapshot.apply(AdapterEvent::OlderHistoryLoaded {
            protocol: ProtocolId::Signal,
            conversation_id: chat.into(),
            before_message_id: "9000:ada".into(),
            more: false,
            note: None,
            topic: None,
            store_cursor: Some(HistoryStoreCursor {
                before_ms: 1,
                skipped: 5,
                resume: false,
                runs: vec![
                    thinwire_protocol::HistorySkipRun {
                        after_id: Some("8000:ada".into()),
                        skipped: 2,
                    },
                    thinwire_protocol::HistorySkipRun {
                        after_id: Some("missing".into()),
                        skipped: 3,
                    },
                ],
            }),
        });
        let runs = snapshot.unreadable_runs();
        assert_eq!(runs.len(), 2);
        assert!(
            runs.iter()
                .all(|run| run.before_id.as_deref() != Some("missing")),
            "an unknown id is not a draw position"
        );
        assert_eq!(
            runs.iter()
                .find(|run| run.before_id.as_deref() == Some("6000:ada"))
                .map(|run| run.skipped),
            Some(3),
            "the unknown count joins the oldest message"
        );
        assert_eq!(
            runs.iter()
                .find(|run| run.before_id.as_deref() == Some("8000:ada"))
                .map(|run| run.skipped),
            Some(2)
        );
        assert_eq!(
            runs.iter().map(|run| run.skipped).sum::<u32>(),
            5,
            "the count is not dropped"
        );
    }

    // endregion: #135
}

#[cfg(test)]
mod account_flow_tests {
    use super::test_support::{ready_with_chats, seed_override};
    use super::*;

    fn telegram(slot: u8) -> ProtocolId {
        ProtocolId::account(ProtocolKind::Telegram, slot).expect("slot")
    }

    /// The row of a second account sits after the first. It removes again,
    /// and the next number is the lowest free one.
    #[test]
    fn a_second_account_has_its_own_row() {
        let mut snapshot = Snapshot::new();
        assert_eq!(
            snapshot.next_account(ProtocolKind::Telegram),
            Some(telegram(1))
        );
        assert!(snapshot.add_account_row(telegram(1)));
        assert!(
            !snapshot.add_account_row(telegram(1)),
            "one row per account"
        );
        assert!(
            !snapshot.add_account_row(ProtocolId::Telegram),
            "account 0 has its row"
        );
        let ids = snapshot.account_ids();
        let first = ids
            .iter()
            .position(|id| *id == ProtocolId::Telegram)
            .expect("first");
        assert_eq!(ids[first + 1], telegram(1));
        assert_eq!(
            snapshot.next_account(ProtocolKind::Telegram),
            Some(telegram(2))
        );
        snapshot.remove_account_row(telegram(1));
        assert!(!snapshot.account_ids().contains(&telegram(1)));
        snapshot.remove_account_row(ProtocolId::Telegram);
        assert!(snapshot.account_ids().contains(&ProtocolId::Telegram));
    }

    /// A login of a second Telegram account, while account 0 is linked: its
    /// steps go to that account, and its Ready links its row. The login
    /// screens are then for account 0 again, which stays linked.
    #[test]
    fn a_second_telegram_account_signs_in_beside_the_first() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        assert!(snapshot.telegram_ready());
        let second = telegram(1);
        let second_store = SecretStore::memory_for_account(1);
        seed_override(&second_store);
        assert!(snapshot.add_account_row(second));
        snapshot.open_login(second, &second_store);
        assert_eq!(snapshot.telegram_login_account(), second);
        assert_eq!(snapshot.auth, AuthScreen::TelegramConnecting);
        let commands = snapshot.take_commands();
        assert!(
            commands.iter().any(|command| matches!(
                command,
                AdapterCommand::ToAccount { account, command }
                    if *account == second && matches!(**command, AdapterCommand::TelegramAuth { .. })
            )),
            "the login step goes to the second account: {commands:?}"
        );
        // A phase of account 0 does not change the screens of the second.
        snapshot.apply(AdapterEvent::TelegramAuth {
            phase: TelegramAuthPhase::Ready,
        });
        assert_eq!(snapshot.auth, AuthScreen::TelegramConnecting);
        assert!(snapshot.protocol_is_linked(ProtocolId::Telegram));
        snapshot.apply(AdapterEvent::FromAccount {
            account: second,
            event: Box::new(AdapterEvent::TelegramAuth {
                phase: TelegramAuthPhase::NeedPhone,
            }),
        });
        assert_eq!(snapshot.auth, AuthScreen::TelegramPhone);
        snapshot.apply(AdapterEvent::FromAccount {
            account: second,
            event: Box::new(AdapterEvent::TelegramAuth {
                phase: TelegramAuthPhase::Ready,
            }),
        });
        assert!(snapshot.protocol_is_linked(second));
        assert!(snapshot.has_session(second));
        assert_eq!(snapshot.auth, AuthScreen::Idle);
        assert_eq!(snapshot.telegram_login_account(), ProtocolId::Telegram);
        assert!(
            snapshot.protocol_is_linked(ProtocolId::Telegram),
            "account 0 stays"
        );
        assert!(snapshot.telegram_ready());
        // The chats of each account stay apart.
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: Conversation {
                protocol: second,
                ..test_support::telegram_chat(1, "Second Ada", 3)
            },
        });
        snapshot.select_protocol(second);
        assert_eq!(snapshot.visible_conversations().len(), 1);
        assert_eq!(snapshot.visible_conversations()[0].title, "Second Ada");
        snapshot.select_protocol(ProtocolId::Telegram);
        assert!(
            snapshot
                .visible_conversations()
                .iter()
                .all(|row| row.title != "Second Ada")
        );
    }

    /// A saved session of a second account resumes with no screen. A lost
    /// session unlinks only that row and stops its client.
    #[test]
    fn a_second_account_resumes_and_ends_in_the_background() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        let second = telegram(1);
        assert!(snapshot.add_account_row(second));
        snapshot.apply(AdapterEvent::FromAccount {
            account: second,
            event: Box::new(AdapterEvent::TelegramAuth {
                phase: TelegramAuthPhase::Ready,
            }),
        });
        assert!(snapshot.protocol_is_linked(second));
        assert_eq!(snapshot.auth, AuthScreen::Idle);
        assert!(snapshot.telegram_ready(), "account 0 is not touched");
        snapshot.take_commands();
        snapshot.apply(AdapterEvent::FromAccount {
            account: second,
            event: Box::new(AdapterEvent::TelegramAuth {
                phase: TelegramAuthPhase::NeedPhone,
            }),
        });
        assert!(!snapshot.protocol_is_linked(second));
        assert_eq!(snapshot.auth, AuthScreen::Idle, "no phone screen opens");
        assert!(snapshot.protocol_is_linked(ProtocolId::Telegram));
        let commands = snapshot.take_commands();
        assert!(commands.contains(&AdapterCommand::Disconnect { protocol: second }));
        let row = snapshot
            .accounts
            .iter()
            .find(|row| row.caps.id == second)
            .expect("row");
        assert_eq!(row.detail, ACCOUNT_SIGN_IN_AGAIN);
    }

    /// A failed start of a linked background account keeps it linked and
    /// sends no Disconnect.
    #[test]
    fn a_failed_background_start_keeps_the_link() {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        let second = telegram(1);
        assert!(snapshot.add_account_row(second));
        for phase in [TelegramAuthPhase::Ready, TelegramAuthPhase::Failed] {
            snapshot.apply(AdapterEvent::FromAccount {
                account: second,
                event: Box::new(AdapterEvent::TelegramAuth { phase }),
            });
        }
        assert!(snapshot.protocol_is_linked(second));
        assert!(
            !snapshot
                .take_commands()
                .iter()
                .any(|command| matches!(command, AdapterCommand::Disconnect { .. }))
        );
    }

    /// A resume of a second account sends its first login step there.
    #[test]
    fn a_saved_second_account_resumes_with_its_own_store() {
        let mut snapshot = Snapshot::new();
        let second = telegram(1);
        let store = SecretStore::memory_for_account(1);
        seed_override(&store);
        store.set(SecretKey::Session, "1").expect("session");
        snapshot.resume_account(second, &store);
        let commands = snapshot.take_commands();
        if tdlib_compiled() {
            assert!(matches!(
                commands.as_slice(),
                [AdapterCommand::ToAccount { account, .. }] if *account == second
            ));
        } else {
            assert!(commands.is_empty(), "no TDLib, no resume");
        }
        let empty = SecretStore::memory_for_account(1);
        seed_override(&empty);
        snapshot.resume_account(second, &empty);
        assert!(snapshot.take_commands().is_empty(), "no saved session");

        // No marker, but the TDLib database of the account and its key.
        let unmarked = SecretStore::memory_for_account(1);
        seed_override(&unmarked);
        unmarked
            .set(SecretKey::DbEncryption, "db-key")
            .expect("database key");
        snapshot.resume_account(second, &unmarked);
        assert!(snapshot.take_commands().is_empty(), "no database found");
        unmarked.set_tdlib_database_for_test(true);
        snapshot.resume_account(second, &unmarked);
        assert_eq!(
            snapshot.take_commands().len(),
            usize::from(tdlib_compiled()),
            "the database resumes the account"
        );
    }
}

#[cfg(test)]
#[cfg(any(feature = "whatsapp-web", feature = "signal-local"))]
mod helper_account_tests {
    use super::*;

    fn second(kind: ProtocolKind) -> ProtocolId {
        ProtocolId::account(kind, 1).expect("slot")
    }

    fn routed(commands: &[AdapterCommand], account: ProtocolId) -> bool {
        commands.iter().any(|command| {
            matches!(command, AdapterCommand::ToAccount { account: to, .. } if *to == account)
        })
    }

    /// The gate of a second WhatsApp account: its acceptance goes to that
    /// account. Its link closes the pair screen, and the screens are for
    /// account 0 again. Account 0 stays linked.
    #[cfg(feature = "whatsapp-web")]
    #[test]
    fn a_second_whatsapp_account_links_with_its_own_gate() {
        let store = SecretStore::memory();
        let mut snapshot = Snapshot::new();
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::WhatsApp,
            state: AccountState::Linked,
        });
        let account = second(ProtocolKind::WhatsApp);
        assert!(snapshot.add_account_row(account));
        snapshot.take_commands();
        snapshot.open_login(account, &store);
        assert_eq!(snapshot.whatsapp_link_account(), account);
        assert_eq!(snapshot.whatsapp_screen, WhatsAppScreen::RiskGate);
        snapshot.acknowledge_whatsapp_risk();
        assert_eq!(snapshot.whatsapp_screen, WhatsAppScreen::Pair);
        let commands = snapshot.take_commands();
        assert!(routed(&commands, account), "{commands:?}");
        // A link of account 0 in the background does not close this screen.
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::WhatsApp,
            state: AccountState::Linked,
        });
        assert_eq!(snapshot.whatsapp_screen, WhatsAppScreen::Pair);
        snapshot.apply(AdapterEvent::Account {
            protocol: account,
            state: AccountState::Linked,
        });
        assert_eq!(snapshot.whatsapp_screen, WhatsAppScreen::Hidden);
        assert_eq!(snapshot.whatsapp_link_account(), ProtocolId::WhatsApp);
        assert!(snapshot.protocol_is_linked(account));
        assert!(snapshot.protocol_is_linked(ProtocolId::WhatsApp));
    }

    /// A background link of a second WhatsApp account does not close the
    /// pair screen of account 0.
    #[cfg(feature = "whatsapp-web")]
    #[test]
    fn a_background_whatsapp_account_leaves_the_screen_of_account_0() {
        let store = SecretStore::memory();
        let mut snapshot = Snapshot::new();
        let account = second(ProtocolKind::WhatsApp);
        assert!(snapshot.add_account_row(account));
        snapshot.open_login(ProtocolId::WhatsApp, &store);
        snapshot.acknowledge_whatsapp_risk();
        assert_eq!(snapshot.whatsapp_screen, WhatsAppScreen::Pair);
        snapshot.apply(AdapterEvent::Account {
            protocol: account,
            state: AccountState::Linked,
        });
        assert_eq!(snapshot.whatsapp_screen, WhatsAppScreen::Pair);
        assert!(snapshot.protocol_is_linked(account));
        assert!(!snapshot.protocol_is_linked(ProtocolId::WhatsApp));
    }

    /// A saved helper account starts its helper at start with no screen.
    #[test]
    fn a_saved_helper_account_resumes_with_no_screen() {
        let mut snapshot = Snapshot::new();
        for kind in [ProtocolKind::WhatsApp, ProtocolKind::Signal] {
            let account = second(kind);
            assert!(snapshot.add_account_row(account));
            snapshot.resume_helper_account(account);
            let commands = snapshot.take_commands();
            assert!(routed(&commands, account), "{kind:?}: {commands:?}");
        }
        snapshot.resume_helper_account(ProtocolId::WhatsApp);
        assert!(
            snapshot.take_commands().is_empty(),
            "account 0 has its own resume"
        );
    }

    /// A second Signal account shows the notice even when account 0
    /// accepted it: its own helper needs the acceptance. Its cancel keeps
    /// the saved acceptance of account 0.
    #[cfg(feature = "signal-local")]
    #[test]
    fn a_second_signal_account_shows_the_notice_and_keeps_the_saved_one() {
        let store = SecretStore::memory();
        let mut snapshot = Snapshot::new();
        snapshot.restore_signal_notice();
        snapshot.take_commands();
        let account = second(ProtocolKind::Signal);
        assert!(snapshot.add_account_row(account));
        snapshot.open_login(account, &store);
        assert_eq!(snapshot.signal_link_account(), account);
        assert_eq!(snapshot.signal_screen, SignalScreen::Notice);
        snapshot.acknowledge_signal_notice();
        assert_eq!(snapshot.signal_screen, SignalScreen::Link);
        let commands = snapshot.take_commands();
        assert!(routed(&commands, account), "{commands:?}");
        snapshot.cancel_signal_link();
        assert!(
            !snapshot.take_signal_notice_forget(),
            "account 0 keeps its acceptance"
        );
        assert_eq!(snapshot.signal_link_account(), ProtocolId::Signal);
        assert!(snapshot
            .take_commands()
            .iter()
            .any(|command| matches!(command, AdapterCommand::ToAccount { account: to, .. } if *to == account)));
    }
}
