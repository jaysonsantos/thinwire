//! App state. Mutated only on the frontend thread from adapter events and user actions.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

#[cfg(feature = "whatsapp-web")]
use thinwire_protocol::WhatsAppPhoneVault;
use thinwire_protocol::{
    AccountState, AdapterCommand, AdapterEvent, AdapterStatus, ChatMessage, Conversation, Delivery,
    DiscordAdapter, ProtocolCapabilities, ProtocolId, TelegramApiSource, TelegramAuthError,
    TelegramAuthPhase, TelegramAuthStep, TelegramCodeVia, TelegramSecretVault, catalog,
    telegram_api_available,
};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboxFilter {
    All,
    Telegram,
    #[cfg(feature = "slack-oauth")]
    Slack,
}

impl InboxFilter {
    /// Top-bar filters for this build. Spike tabs stay out until their feature is on.
    #[must_use]
    pub fn chrome_filters() -> &'static [Self] {
        #[cfg(feature = "slack-oauth")]
        {
            const FILTERS: &[InboxFilter] =
                &[InboxFilter::All, InboxFilter::Telegram, InboxFilter::Slack];
            FILTERS
        }
        #[cfg(not(feature = "slack-oauth"))]
        {
            const FILTERS: &[InboxFilter] = &[InboxFilter::All, InboxFilter::Telegram];
            FILTERS
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Telegram => "Telegram",
            #[cfg(feature = "slack-oauth")]
            Self::Slack => "Slack",
        }
    }

    #[must_use]
    pub const fn matches(self, protocol: ProtocolId) -> bool {
        match self {
            Self::All => true,
            Self::Telegram => matches!(protocol, ProtocolId::Telegram),
            #[cfg(feature = "slack-oauth")]
            Self::Slack => matches!(protocol, ProtocolId::Slack),
        }
    }

    /// Account chip visibility for the current filter. Off-feature spikes stay out.
    ///
    /// Cargo features decide which protocols exist. Callers still AND
    /// [`Snapshot::account_surface_visible`].
    #[must_use]
    pub const fn shows_in_switcher(self, protocol: ProtocolId) -> bool {
        self.matches(protocol)
    }
}

/// Compile-time chrome gate: spikes stay invisible when their feature is off.
#[must_use]
pub const fn protocol_chrome_enabled(protocol: ProtocolId) -> bool {
    match protocol {
        ProtocolId::Telegram => true,
        ProtocolId::Slack => cfg!(feature = "slack-oauth"),
        ProtocolId::WhatsApp => cfg!(feature = "whatsapp-web"),
        ProtocolId::Discord => cfg!(feature = "discord-bot"),
        ProtocolId::Signal => cfg!(feature = "signal-local"),
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
pub const LOADING_MESSAGES_STATUS: &str = "Loading messages…";
pub const LOADING_OLDER_STATUS: &str = "Loading older messages…";
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
}

/// Local-only Signal screens. Only the `signal-local` build can enter them.
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

#[derive(Debug, Clone)]
pub struct AccountRow {
    pub caps: ProtocolCapabilities,
    pub status: AdapterStatus,
    pub detail: String,
    /// Link state. Only `AdapterEvent::Account` (and the Telegram login,
    /// which maps onto it) changes it. A `Status` never does.
    pub state: AccountState,
}

impl AccountRow {
    /// Signed in: the shell shows this protocol's chats.
    #[must_use]
    pub fn linked(&self) -> bool {
        self.state == AccountState::Linked
    }
}

/// Why `sync_focused_row` runs. A search change or a key move always scrolls.
/// A list change scrolls only when the highlight's visible index changes.
enum FocusFollow {
    Query,
    List,
    Moved,
}

/// Text the adapter rejected for one chat. A compose send and a retry are
/// separate: the compose text wins when it still matches, otherwise the
/// retry text fills an empty draft.
#[derive(Default)]
struct RejectedBody {
    compose: Option<String>,
    row: Option<String>,
}

/// App state. `Debug` is manual: it prints no secret and no user text.
pub struct Snapshot {
    pub accounts: Vec<AccountRow>,
    conversations: HashMap<ProtocolId, Vec<Conversation>>,
    messages: HashMap<(ProtocolId, String), Vec<ChatMessage>>,
    pub selected_protocol: ProtocolId,
    pub selected_conversation: Option<String>,
    /// Keyboard highlight. Arrows move this. Enter and a click open the chat.
    pub focused_row: Option<String>,
    pub filter: InboxFilter,
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
    /// When the UI first saw the keychain read still running.
    keychain_wait_started: Option<Instant>,
    /// The last keychain read failed. Copied from the store each frame.
    keychain_failed: bool,
    /// The user asked to read the keychain again. The app runs it off the UI thread.
    keychain_retry: bool,
    /// Protocols whose chat list is loading.
    chat_list_loading: HashSet<ProtocolId>,
    /// Chats whose history is loading, per protocol.
    history_loading: HashSet<(ProtocolId, String)>,
    /// The last note of each protocol (`AdapterEvent::Notice`). Never an error.
    notices: HashMap<ProtocolId, String>,
    /// The chat the adapters were last told the user looks at (`ViewChat`).
    viewed: Option<(ProtocolId, String)>,
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
    /// Telegram chats with a request for older messages in flight (#30).
    /// One request at a time for each chat.
    older_loading: HashSet<String>,
    /// Telegram chats whose start is loaded. No more older requests (#30).
    older_at_start: HashSet<String>,
    /// Telegram chats whose last older request brought nothing older: the
    /// anchor it used and when it ended. That anchor waits
    /// `OLDER_RETRY_DELAY` (#61 review).
    older_retry: HashMap<String, (String, Instant, u32)>,
    scroll_to_selected: bool,
    scroll_to_focused: bool,
    /// Inbox row ids last seen by `sync_focused_row`. A list change is a difference here.
    seen_visible_ids: Vec<String>,
    /// Unsent compose text per chat. `compose` holds the selected chat's draft.
    drafts: HashMap<(ProtocolId, String), String>,
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
    /// The id of the pairing that runs now. A QR or pair code of any other
    /// pairing is dropped (shell plan 12). `None`: no pairing runs.
    #[cfg(any(feature = "whatsapp-web", feature = "signal-local"))]
    pairing_generation: Option<u64>,
    /// Name of the folder the worker moved aside. Shown on the next phone step.
    data_reset: Option<String>,
    api_source: TelegramApiSource,
    pending: Vec<AdapterCommand>,
    keychain_flush: bool,
    #[cfg(feature = "whatsapp-web")]
    pub whatsapp_screen: WhatsAppScreen,
    #[cfg(feature = "whatsapp-web")]
    pub whatsapp_phone: String,
    #[cfg(feature = "whatsapp-web")]
    pub whatsapp_qr: Option<String>,
    #[cfg(feature = "whatsapp-web")]
    pub whatsapp_pair_code: Option<String>,
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
    /// The user accepted the full-screen local-build notice in this session.
    /// Linking needs it; showing the notice again or cancelling clears it.
    #[cfg(feature = "signal-local")]
    signal_notice_acknowledged: bool,
}

/// Redacted: login fields, the WhatsApp phone and pairing material, the
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
            .field("filter", &self.filter)
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
            .field("whatsapp_started", &self.whatsapp_started);
        #[cfg(feature = "signal-local")]
        out.field("signal_screen", &self.signal_screen)
            .field("signal_started", &self.signal_started);
        out.finish_non_exhaustive()
    }
}

impl Default for Snapshot {
    fn default() -> Self {
        Self::new()
    }
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
            })
            .collect();
        Self {
            accounts,
            conversations: HashMap::new(),
            messages: HashMap::new(),
            selected_protocol: ProtocolId::Telegram,
            selected_conversation: None,
            focused_row: None,
            filter: InboxFilter::All,
            search: String::new(),
            auth: AuthScreen::Idle,
            telegram_api_id: String::new(),
            telegram_api_hash: String::new(),
            telegram_phone: String::new(),
            telegram_code: String::new(),
            telegram_2fa: String::new(),
            error: None,
            status_text: "Sign in with Telegram to get started.".into(),
            ready_status: None,
            compose: String::new(),
            auth_busy: false,
            auth_notice: None,
            auth_rejection: None,
            code_via: None,
            telegram_authorized: false,
            resume: Resume::Waiting,
            keychain_wait_started: None,
            keychain_failed: false,
            keychain_retry: false,
            chat_list_loading: HashSet::new(),
            history_loading: HashSet::new(),
            older_loading: HashSet::new(),
            older_at_start: HashSet::new(),
            older_retry: HashMap::new(),
            notices: HashMap::new(),
            viewed: None,
            timed_out: Vec::new(),
            timeout_error: None,

            open_on_link: None,
            sessions: HashSet::new(),
            extra_visible: HashSet::new(),
            scroll_to_selected: false,
            scroll_to_focused: false,
            seen_visible_ids: Vec::new(),
            drafts: HashMap::new(),
            rejected_bodies: HashMap::new(),
            focus_compose: false,
            stopped: HashSet::new(),
            sends: SendTracker::default(),
            #[cfg(any(feature = "whatsapp-web", feature = "signal-local"))]
            next_pairing_generation: 1,
            #[cfg(any(feature = "whatsapp-web", feature = "signal-local"))]
            pairing_generation: None,
            data_reset: None,
            api_source: TelegramApiSource::from_build(),
            pending: Vec::new(),
            keychain_flush: false,
            #[cfg(feature = "whatsapp-web")]
            whatsapp_screen: WhatsAppScreen::Hidden,
            #[cfg(feature = "whatsapp-web")]
            whatsapp_phone: String::new(),
            #[cfg(feature = "whatsapp-web")]
            whatsapp_qr: None,
            #[cfg(feature = "whatsapp-web")]
            whatsapp_pair_code: None,
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
            signal_notice_acknowledged: false,
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
        // Inbox events come only while the account is linked. One that
        // arrives while it is not is from a cancelled or ended client, for
        // example queued before Cancel: drop it (PR #49 review). Adapters
        // send `Account { Linked }` before their first inbox event.
        if let Some(protocol) = event.inbox_protocol()
            && !self.has_session(protocol)
        {
            return;
        }
        match event {
            AdapterEvent::Status {
                protocol,
                status,
                detail,
            } => {
                // The status line only. It never changes the link state: a
                // recoverable error must not hide the inbox (shell plan 1, 11).
                if let Some(row) = self.accounts.iter_mut().find(|row| row.caps.id == protocol) {
                    row.status = status;
                    row.detail = detail.clone();
                }
                if matches!(status, AdapterStatus::Error | AdapterStatus::Refused) {
                    // A failed load does not send its end event. Stop the spinners.
                    self.stop_spinners(protocol, None);
                    #[cfg(feature = "signal-local")]
                    if protocol == ProtocolId::Signal
                        && self.signal_started
                        && !self.protocol_linked(ProtocolId::Signal)
                    {
                        self.set_error("Signal linking failed.", &detail, "Start linking again.");
                    }
                }
                // Crate / feature jargon stays on the account row and in logs.
                // Chrome surfaces Telegram errors and Ready operational copy only.
                if protocol == ProtocolId::Telegram
                    && matches!(
                        status,
                        AdapterStatus::Error | AdapterStatus::Refused | AdapterStatus::Ready
                    )
                {
                    self.ready_status = (status == AdapterStatus::Ready).then(|| detail.clone());
                    self.status_text = detail;
                    if matches!(status, AdapterStatus::Error | AdapterStatus::Refused) {
                        if self.auth != AuthScreen::Idle {
                            self.auth_busy = false;
                        }
                        self.resume = Resume::Settled;
                    }
                }
            }
            AdapterEvent::ConversationUpsert { conversation } => {
                let protocol = conversation.protocol;
                let selected = (self.selected_protocol == protocol)
                    .then(|| self.selected_conversation.clone())
                    .flatten();
                {
                    let list = self.conversations.entry(protocol).or_default();
                    let before = selected
                        .as_ref()
                        .and_then(|id| list.iter().position(|row| row.id == *id));
                    if let Some(existing) = list.iter_mut().find(|row| row.id == conversation.id) {
                        *existing = conversation;
                    } else {
                        list.push(conversation);
                    }
                    sort_conversations(list);
                    let after = selected
                        .as_ref()
                        .and_then(|id| list.iter().position(|row| row.id == *id));
                    if before.is_some() && before != after {
                        self.scroll_to_selected = true;
                    }
                }
                self.ensure_conversation_selection();
                self.sync_focused_row(FocusFollow::List);
            }
            AdapterEvent::MessageDelivery {
                protocol,
                conversation_id,
                message_id,
                delivery,
            } => self.set_delivery(protocol, &conversation_id, &message_id, delivery),
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
            AdapterEvent::OlderHistoryLoaded {
                protocol,
                conversation_id,
                before_message_id,
                more,
                ..
            } => {
                if matches!(protocol, ProtocolId::Telegram | ProtocolId::Signal) {
                    self.older_loading.remove(&conversation_id);
                    let oldest = self
                        .messages
                        .get(&(protocol, conversation_id.clone()))
                        .and_then(|list| list.first())
                        .map(|row| row.id.as_str());
                    if !more {
                        self.older_retry.remove(&conversation_id);
                        self.older_at_start.insert(conversation_id);
                    } else if oldest == Some(before_message_id.as_str()) {
                        // Nothing older came: do not ask this anchor at once.
                        // A repeat on the same anchor waits longer (#67).
                        let tries = match self.older_retry.get(&conversation_id) {
                            Some((anchor, _, tries)) if *anchor == before_message_id => tries + 1,
                            _ => 1,
                        };
                        self.older_retry
                            .insert(conversation_id, (before_message_id, Instant::now(), tries));
                    } else {
                        self.older_retry.remove(&conversation_id);
                    }
                }
            }
            AdapterEvent::ChatListLoaded { protocol } => {
                self.chat_list_loading.remove(&protocol);
            }
            AdapterEvent::HistoryLoaded {
                protocol,
                conversation_id,
            } => {
                self.history_loading.remove(&(protocol, conversation_id));
            }
            AdapterEvent::ConversationRemoved { protocol, id } => {
                self.remove_conversation(protocol, &id);
            }
            AdapterEvent::MessageReceived { message } => {
                let before =
                    self.delivery_of(message.protocol, &message.conversation_id, &message.id);
                self.note_delivery(before, &message);
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
            } => self.patch_message_body(protocol, &conversation_id, &message_id, body),
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
                if self.current_pairing(generation) {
                    self.whatsapp_qr = Some(code.reveal().to_string());
                }
                #[cfg(not(feature = "whatsapp-web"))]
                {
                    let _ = (code, generation);
                }
            }
            AdapterEvent::WhatsAppPairCode { code, generation } => {
                #[cfg(feature = "whatsapp-web")]
                if self.current_pairing(generation) {
                    self.whatsapp_pair_code = Some(code.reveal().to_string());
                }
                #[cfg(not(feature = "whatsapp-web"))]
                {
                    let _ = (code, generation);
                }
            }
            AdapterEvent::SignalQr { code, generation } => {
                #[cfg(feature = "signal-local")]
                if self.current_pairing(generation) {
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
        self.pending.push(command);
    }

    pub fn take_commands(&mut self) -> Vec<AdapterCommand> {
        std::mem::take(&mut self.pending)
    }

    #[must_use]
    pub fn take_keychain_flush(&mut self) -> bool {
        std::mem::take(&mut self.keychain_flush)
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

    fn try_resume(&mut self, store: &SecretStore, live: bool) {
        if self.resume != Resume::Waiting || !store.attach_settled() {
            return;
        }
        let saved_session = store.get(SecretKey::Session).ok().flatten().is_some();
        if !live
            || !saved_session
            || self.auth != AuthScreen::Idle
            || self.telegram_authorized
            || !self.has_api_credentials(store)
        {
            self.resume = Resume::Settled;
            return;
        }
        self.resume = Resume::Connecting;
        self.queue_telegram_step(TelegramAuthStep::ApiCredentials);
        self.status_text = RESUME_CONNECTING.into();
    }

    #[must_use]
    pub fn center_view(&self) -> CenterView {
        if self.auth != AuthScreen::Idle {
            return CenterView::Auth;
        }
        // Any linked protocol shows the inbox, not only Telegram (shell plan 2).
        if self.any_linked() {
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
        self.focused_row = None;
        self.ensure_conversation_selection();
    }

    pub fn select_conversation(&mut self, id: String) {
        self.focused_row = Some(id.clone());
        self.set_selected_conversation(Some(id));
        self.focus_compose = true;
        self.queue_open_chat();
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
    pub fn set_draft(&mut self, protocol: ProtocolId, chat: &str, text: String) {
        if self.is_selected_chat(protocol, chat) {
            self.compose = text;
        } else if text.is_empty() {
            self.drafts.remove(&(protocol, chat.to_owned()));
        } else {
            self.drafts.insert((protocol, chat.to_owned()), text);
        }
    }

    fn set_selected_conversation(&mut self, id: Option<String>) {
        if self.selected_conversation == id {
            return;
        }
        let draft = std::mem::take(&mut self.compose);
        if let Some(old) = self.selected_conversation.take()
            && !draft.is_empty()
        {
            self.drafts.insert((self.selected_protocol, old), draft);
        }
        if let Some(new) = id.as_ref() {
            self.compose = self
                .drafts
                .remove(&(self.selected_protocol, new.clone()))
                .unwrap_or_default();
        }
        self.selected_conversation = id;
    }

    /// Every adapter answered `Shutdown` with `Stopped`: no session of any
    /// protocol (TDLib, the WhatsApp bot and its SQLite session) still runs.
    #[must_use]
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
        sends_text
            && !self.compose.trim().is_empty()
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
        if !sends_text || !writable {
            return;
        }
        let Some(message) = self
            .messages
            .get_mut(&(protocol, conversation_id.clone()))
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
        self.status_text = "Sending…".into();
        self.pending.push(AdapterCommand::ResendMessage {
            protocol,
            conversation_id,
            message_id: message_id.to_string(),
            request,
        });
    }

    pub fn set_filter(&mut self, filter: InboxFilter) {
        if self.filter == filter {
            return;
        }
        self.filter = filter;
        if !filter.matches(self.selected_protocol)
            && let Some(first) = self
                .accounts
                .iter()
                .find(|row| {
                    filter.matches(row.caps.id) && self.account_surface_visible(row.caps.id)
                })
                .map(|row| row.caps.id)
        {
            self.select_protocol(first);
        }
        self.sync_focused_row(FocusFollow::List);
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
        if self.extra_visible.contains(&protocol) {
            return true;
        }
        match protocol {
            ProtocolId::Discord => self.discord_inbox_visible(),
            other => protocol_chrome_enabled(other),
        }
    }

    #[must_use]
    pub fn shows_in_switcher(&self, protocol: ProtocolId) -> bool {
        self.filter.shows_in_switcher(protocol) && self.account_surface_visible(protocol)
    }

    pub fn visible_conversations(&self) -> Vec<&Conversation> {
        if !self.account_surface_visible(self.selected_protocol)
            || !self.has_session(self.selected_protocol)
        {
            return Vec::new();
        }
        let query = self.search.trim().to_ascii_lowercase();
        self.conversations
            .get(&self.selected_protocol)
            .into_iter()
            .flatten()
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

    /// Telegram after Ready, and Signal after the account is linked.
    fn pages_older(&self) -> bool {
        match self.selected_protocol {
            ProtocolId::Telegram => self.telegram_authorized,
            ProtocolId::Signal => self.protocol_linked(ProtocolId::Signal),
            _ => false,
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

    /// The status line is a finished success: the adapter sent it with
    /// `AdapterStatus::Ready`, nothing replaced it since, and no load runs.
    /// The kind comes from the adapter status and the load state, not from
    /// the text (#56).
    #[must_use]
    pub fn status_is_idle(&self) -> bool {
        !self.is_loading() && self.ready_status.as_deref() == Some(self.status_text.as_str())
    }

    /// A chat list, a history page, or a send is still running. A `Ready`
    /// line such as "Loading recent messages." is not idle then (#64 review).
    #[must_use]
    pub fn is_loading(&self) -> bool {
        !self.chat_list_loading.is_empty()
            || !self.history_loading.is_empty()
            || !self.older_loading.is_empty()
            || !self.sends.is_empty()
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
        } else if protocol == ProtocolId::Telegram && !self.older_loading.is_empty() {
            Some(LOADING_OLDER_STATUS)
        } else if self.sends.any_for(protocol) {
            Some(SENDING_STATUS)
        } else {
            None
        }
    }

    /// The line the status strip shows (#80). While any load runs it is a
    /// running load's line, never a finished Ready line: the selected
    /// protocol's load first, then another visible protocol's load, named.
    /// With no load, it is the last status line.
    #[must_use]
    pub fn status_line(&self) -> std::borrow::Cow<'_, str> {
        if let Some(line) = self.loading_line(self.selected_protocol) {
            return line.into();
        }
        let other = self
            .accounts
            .iter()
            .map(|row| row.caps.id)
            .filter(|id| *id != self.selected_protocol && self.account_surface_visible(*id))
            .find_map(|id| self.loading_line(id).map(|line| (id, line)));
        match other {
            Some((id, line)) => format!("{}: {line}", id.display_name()).into(),
            None => self.status_text.as_str().into(),
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

    /// Older-message paging of the selected chat (#30).
    #[must_use]
    pub fn older_state(&self) -> OlderState {
        let Some(id) = self.selected_conversation.as_ref() else {
            return OlderState::Idle;
        };
        if !self.pages_older() {
            return OlderState::Idle;
        }
        if self.older_loading.contains(id) {
            OlderState::Loading
        } else if self.older_at_start.contains(id) {
            OlderState::StartOfChat
        } else {
            OlderState::Idle
        }
    }

    /// Ask for the page before the oldest loaded message of the selected
    /// chat. Telegram and a linked Signal account answer `LoadOlderMessages`.
    /// Nothing goes out while a request runs, the first page loads, or the
    /// start is loaded.
    pub(crate) fn load_older(&mut self) {
        self.load_older_at(Instant::now());
    }

    /// `load_older` with the clock as a parameter, for tests. An anchor that
    /// brought nothing older waits `OLDER_RETRY_DELAY` from `now`.
    fn load_older_at(&mut self, now: Instant) {
        let Some((id, oldest)) = self.older_anchor_at(now) else {
            return;
        };
        let protocol = self.selected_protocol;
        self.older_loading.insert(id.clone());
        self.pending.push(AdapterCommand::LoadOlderMessages {
            protocol,
            conversation_id: id,
            before_message_id: oldest,
        });
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
        if self.history_loading.contains(&(protocol, id.clone()))
            || self.older_loading.contains(&id)
            || self.older_at_start.contains(&id)
        {
            return None;
        }
        let oldest = self.selected_messages().first()?.id.clone();
        if let Some((anchor, at, tries)) = self.older_retry.get(&id)
            && *anchor == oldest
            && now.saturating_duration_since(*at) < older_wait(*tries)
        {
            return None;
        }
        Some((id, oldest))
    }

    /// True once after the selected row moved in the sorted list.
    /// A scroll request waits for the inbox rows. Read-only; see `take_scroll_to_selected`.
    #[must_use]
    pub const fn wants_scroll_to_selected(&self) -> bool {
        self.scroll_to_selected
    }

    pub fn take_scroll_to_selected(&mut self) -> bool {
        std::mem::take(&mut self.scroll_to_selected)
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
            .get(&(self.selected_protocol, id.clone()))
            .map_or(&[], Vec::as_slice)
    }

    pub fn refresh_visible(&mut self) {
        let protocols: HashSet<ProtocolId> = self
            .accounts
            .iter()
            .map(|row| row.caps.id)
            .filter(|id| self.filter.matches(*id) && self.account_surface_visible(*id))
            .collect();
        for protocol in protocols {
            if self.protocol_linked(protocol) {
                self.chat_list_loading.insert(protocol);
                self.pending.push(AdapterCommand::LoadChats { protocol });
            } else if protocol != ProtocolId::Slack {
                // Slack Connect with no token opens the browser. Only the
                // Add Slack workspace button starts that install.
                self.pending.push(AdapterCommand::Connect { protocol });
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

    /// Add account is offered only while Telegram is not signed in: this
    /// build supports one Telegram account (ux F8).
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
        self.pending.push(AdapterCommand::Disconnect {
            protocol: ProtocolId::Telegram,
        });
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
                self.pending.push(AdapterCommand::Disconnect {
                    protocol: ProtocolId::Telegram,
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
            CenterView::Resuming { .. } | CenterView::Thread => {}
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
            Some(Pending::Send { body, .. }) => {
                self.drop_rejected_body(protocol, chat);
                self.clear_sent_text(protocol, chat, &body);
            }
            Some(Pending::Retry { .. }) => self.drop_rejected_body(protocol, chat),
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
        self.drop_rejected_body(protocol, chat);
        match pending {
            Pending::Send { body, .. } => self.clear_sent_text(protocol, chat, &body),
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
    }

    /// Set the timeout error from `timed_out`: one line per expired send, or
    /// no error when the list is empty.
    fn show_timeouts(&mut self) {
        let error = match self.timed_out.as_slice() {
            [] => None,
            [one] => Some(UserError {
                happened: "Message not sent.".into(),
                why: format!("{} did not answer in time.", one.protocol.display_name()),
                next: self.resend_hint(one.protocol, &one.chat, one.retry),
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

    /// Where the unsent text is, and what to do (qa on #81): the compose
    /// field of the chat that shows, or the draft of another chat.
    fn resend_hint(&self, protocol: ProtocolId, chat: &str, retry: bool) -> String {
        let selected = self.is_selected_chat(protocol, chat);
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
    /// it is still exactly that text.
    fn clear_sent_text(&mut self, protocol: ProtocolId, chat: &str, body: &str) {
        let selected = self.is_selected_chat(protocol, chat);
        if selected && self.compose.trim() == body {
            self.compose.clear();
        } else if self
            .drafts
            .get(&(protocol, chat.to_owned()))
            .is_some_and(|draft| draft.trim() == body)
        {
            self.drafts.remove(&(protocol, chat.to_owned()));
        }
    }

    /// Expire sends and retries with no answer after `SEND_TIMEOUT` (#69).
    /// The core calls it on every pump.
    /// Returns true when an entry expired, so the state changed.
    pub(crate) fn expire_sends(&mut self) -> bool {
        self.expire_sends_at(Instant::now())
    }

    /// When the next send or retry expires, if one is in flight.
    pub(crate) fn next_send_deadline(&self) -> Option<Instant> {
        self.sends.next_deadline()
    }

    /// Test hook: age every tracked send by `by`.
    #[cfg(test)]
    pub(crate) fn age_sends_for_test(&mut self, by: Duration) {
        self.sends.age_for_test(by);
    }

    /// `expire_sends` with the clock as a parameter, for tests. An expired
    /// send keeps its text; an expired retry sets its row back to `Failed`.
    fn expire_sends_at(&mut self, now: Instant) -> bool {
        let expired = self.sends.expire(now);
        if expired.is_empty() {
            return false;
        }
        if self.error != self.timeout_error {
            self.timed_out.clear();
        }
        for (protocol, chat, pending) in expired {
            let retry = match &pending {
                Pending::Send { .. } => false,
                Pending::Retry { message_id, .. } => {
                    self.set_delivery(protocol, &chat, message_id, Delivery::Failed);
                    true
                }
            };
            self.timed_out.push(TimedOut {
                protocol,
                chat,
                request: pending.request(),
                retry,
            });
        }
        // Every expired send shows, not only the last one (qa on #81).
        self.show_timeouts();
        true
    }

    /// The adapter rejected this send (chat and request id match): it was
    /// not accepted. The text is still in its compose field or draft. Other
    /// errors (for example a history load error) never fail a send.
    fn fail_unaccepted_send(&mut self, protocol: ProtocolId, chat: &str, request: u64) {
        let why = format!("{} did not accept the message.", protocol.display_name());
        let settled = self.sends.settle(protocol, chat, request);
        if settled.is_none() {
            // A late rejection after the send expired changes nothing: the
            // expiry already failed it (PR #81 review).
            let _ = self.sends.settle_expired(protocol, chat, request);
        }
        match settled {
            Some(Pending::Send { body, .. }) => {
                if !body.is_empty() {
                    self.rejected_bodies
                        .entry((protocol, chat.to_owned()))
                        .or_default()
                        .compose = Some(body);
                }
                let next = self.resend_hint(protocol, chat, false);
                self.set_error("Message not sent.", &why, &next);
            }
            Some(Pending::Retry { message_id, .. }) => {
                self.set_delivery(protocol, chat, &message_id, Delivery::Failed);
                if let Some(text) = self
                    .message_body(protocol, chat, &message_id)
                    .filter(|text| !text.is_empty())
                {
                    self.rejected_bodies
                        .entry((protocol, chat.to_owned()))
                        .or_default()
                        .row = Some(text);
                }
                let next = self.resend_hint(protocol, chat, true);
                self.set_error("Message not sent.", &why, &next);
            }
            None => {}
        }
    }

    /// Forget a rejected send for this chat. An accepted send, a removed
    /// chat, or a session end that no longer shows the text all use this.
    fn drop_rejected_body(&mut self, protocol: ProtocolId, chat: &str) {
        self.rejected_bodies.remove(&(protocol, chat.to_owned()));
    }

    /// Compose-rejected text wins when it still matches. Otherwise a retry's
    /// text fills an empty draft.
    fn text_to_keep(
        &self,
        protocol: ProtocolId,
        chat: &str,
        rejected: &RejectedBody,
    ) -> Option<String> {
        let current = if self.is_selected_chat(protocol, chat) {
            self.compose.trim()
        } else {
            self.drafts
                .get(&(protocol, chat.to_owned()))
                .map_or("", |draft| draft.trim())
        };
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
        self.messages
            .get(&(protocol, chat.to_owned()))
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
        if !self.can_send() {
            return;
        }
        let Some(conversation_id) = self.selected_conversation.clone() else {
            return;
        };
        let body = self.compose.trim().to_string();
        // Keep the text until the adapter accepts the send; see note_send_accepted.
        let protocol = self.selected_protocol;
        let Some(request) =
            self.sends
                .begin_send(protocol, &conversation_id, &body, Instant::now())
        else {
            return;
        };
        self.error = None;
        self.pending.push(AdapterCommand::SendText {
            protocol,
            conversation_id,
            body,
            request,
        });
        self.status_text = "Sending…".into();
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
        self.auth_notice = None;
        if phase == TelegramAuthPhase::NeedPhone {
            // TDLib is back at the phone step: an old code or password is stale.
            self.telegram_code.clear();
            self.telegram_2fa.clear();
        }
        match phase {
            TelegramAuthPhase::NeedPhone if self.data_reset.is_some() => {
                let notice = data_reset_notice(&self.data_reset.take().unwrap_or_default());
                self.auth = AuthScreen::TelegramPhone;
                self.status_text.clone_from(&notice);
                self.auth_notice = Some(notice);
            }
            TelegramAuthPhase::NeedPhone if resuming => {
                // The worker drops the stale session marker on this path.
                self.auth = AuthScreen::TelegramPhone;
                self.auth_notice = Some(SESSION_ENDED_NOTICE.into());
                self.status_text = SESSION_ENDED_NOTICE.into();
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
                    self.pending.push(AdapterCommand::Disconnect {
                        protocol: ProtocolId::Telegram,
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
            .find(|row| row.caps.id == ProtocolId::Telegram)
        {
            row.state = AccountState::Linked;
        }
        self.sessions.insert(ProtocolId::Telegram);
        self.telegram_authorized = true;
        // The worker loads the main list right after Ready.
        self.chat_list_loading.insert(ProtocolId::Telegram);
        // Keep a selection the user made in another linked protocol (#70).
        let keep = self.selected_protocol != ProtocolId::Telegram
            && self.has_session(self.selected_protocol);
        if !keep {
            self.select_protocol(ProtocolId::Telegram);
        }
        self.auth = AuthScreen::Idle;
        self.auth_busy = false;
        self.error = None;
        self.status_text = "Telegram is ready. Loading the chat list.".into();
    }

    /// The live session ended elsewhere. Drop the old inbox, then start a new
    /// client: the next phone step says the session ended (qa R73).
    fn end_telegram_session(&mut self) {
        if !self.telegram_authorized {
            return;
        }
        self.telegram_authorized = false;
        self.end_session(ProtocolId::Telegram);
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
            .find(|row| row.caps.id == ProtocolId::Telegram)
        {
            row.state = AccountState::Unlinked;
        }
        self.sessions.remove(&ProtocolId::Telegram);
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
        self.pending
            .push(AdapterCommand::TelegramAuth { step, epoch: 0 });
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
        if let Some(first) = self
            .conversations
            .get(&self.selected_protocol)
            .and_then(|rows| rows.iter().find(|row| !row.placeholder))
        {
            self.set_selected_conversation(Some(first.id.clone()));
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
        let already = self.pending.iter().any(|command| {
            matches!(
                command,
                AdapterCommand::OpenChat { protocol: owner, conversation_id }
                    if *owner == protocol && conversation_id == &id
            )
        });
        if already {
            return;
        }
        self.history_loading.insert((protocol, id.clone()));
        self.pending.push(AdapterCommand::OpenChat {
            protocol,
            conversation_id: id,
        });
    }

    /// Tell the adapters which chat shows now (shell plan 9). Sends only the
    /// difference: `None` to the protocol the user left, then the new chat.
    /// The core calls it after every dispatch and pump, so every path (a
    /// click, an auto-select, a removed chat, a session end, a login form
    /// over the thread) is covered.
    pub(crate) fn sync_viewed(&mut self) {
        let now = self.viewed_chat();
        if now == self.viewed {
            return;
        }
        let before = std::mem::replace(&mut self.viewed, now.clone());
        let left_protocol = match (&before, &now) {
            (Some((old, _)), Some((new, _))) => (old != new).then_some(*old),
            (Some((old, _)), None) => Some(*old),
            (None, _) => None,
        };
        if let Some(protocol) = left_protocol {
            self.pending.push(AdapterCommand::ViewChat {
                protocol,
                conversation_id: None,
            });
        }
        if let Some((protocol, id)) = now {
            self.pending.push(AdapterCommand::ViewChat {
                protocol,
                conversation_id: Some(id),
            });
        }
    }

    /// The chat the user looks at, as last synced by `sync_viewed`.
    #[must_use]
    pub fn viewed(&self) -> Option<(ProtocolId, &str)> {
        self.viewed
            .as_ref()
            .map(|(protocol, id)| (*protocol, id.as_str()))
    }

    /// One chat row of one protocol.
    #[must_use]
    pub fn conversation(&self, protocol: ProtocolId, id: &str) -> Option<&Conversation> {
        self.conversations
            .get(&protocol)?
            .iter()
            .find(|row| row.id == id)
    }

    /// Unread messages of every chat that is not muted, in every protocol
    /// with a session. For the window title and a taskbar badge (#32).
    #[must_use]
    pub fn unread_total(&self) -> u32 {
        self.conversations
            .iter()
            .filter(|(protocol, _)| self.has_session(**protocol))
            .flat_map(|(_, rows)| rows.iter())
            .filter(|row| !row.muted)
            .map(|row| row.unread)
            .fold(0, u32::saturating_add)
    }

    /// The chat that shows in the thread now, if any.
    fn viewed_chat(&self) -> Option<(ProtocolId, String)> {
        if self.center_view() != CenterView::Thread {
            return None;
        }
        let protocol = self.selected_protocol;
        let id = self.selected_conversation.clone()?;
        let listed = self.has_session(protocol)
            && self.account_surface_visible(protocol)
            && self
                .conversations
                .get(&protocol)
                .is_some_and(|rows| rows.iter().any(|row| row.id == id && !row.placeholder));
        listed.then_some((protocol, id))
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
        if let Some(row) = self.accounts.iter_mut().find(|row| row.caps.id == protocol) {
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
                #[cfg(feature = "whatsapp-web")]
                if protocol == ProtocolId::WhatsApp {
                    self.finish_whatsapp_link();
                }
                #[cfg(feature = "signal-local")]
                if protocol == ProtocolId::Signal {
                    self.finish_signal_link();
                }
            }
            AccountState::Unlinked if had_session || before != AccountState::Unlinked => {
                self.end_session(protocol);
            }
            AccountState::Unlinked =>
            {
                #[cfg(feature = "signal-local")]
                if protocol == ProtocolId::Signal && self.signal_started {
                    self.signal_started = false;
                    self.signal_qr = None;
                }
            }
            AccountState::Linking => {}
        }
    }

    /// The session of one protocol ended (shell plan 8): drop its rows,
    /// messages, spinners, notes, and sends. Another linked protocol takes
    /// the selection. Drafts of this protocol go too, except a rejected send
    /// whose text is still in that chat. A Telegram session end drops those
    /// as well: the next account on this machine must not see them.
    fn end_session(&mut self, protocol: ProtocolId) {
        if let Some(row) = self.accounts.iter_mut().find(|row| row.caps.id == protocol) {
            row.state = AccountState::Unlinked;
        }
        self.sessions.remove(&protocol);
        self.conversations.remove(&protocol);
        let keep: Vec<(String, String)> = if protocol == ProtocolId::Telegram {
            Vec::new()
        } else {
            self.rejected_bodies
                .iter()
                .filter_map(|((owner, chat), rejected)| {
                    if *owner != protocol {
                        return None;
                    }
                    self.text_to_keep(protocol, chat, rejected)
                        .map(|text| (chat.clone(), text))
                })
                .collect()
        };
        self.rejected_bodies
            .retain(|(owner, _), _| *owner != protocol);
        self.drafts.retain(|(owner, _), _| *owner != protocol);
        for (chat, body) in keep {
            self.drafts.insert((protocol, chat), body);
        }
        self.messages.retain(|(owner, _), _| *owner != protocol);
        self.history_loading.retain(|(owner, _)| *owner != protocol);
        self.chat_list_loading.remove(&protocol);
        self.notices.remove(&protocol);
        self.sends.drop_protocol(protocol);
        if protocol == ProtocolId::Telegram {
            self.older_loading.clear();
            self.older_at_start.clear();
            self.older_retry.clear();
        }
        #[cfg(feature = "whatsapp-web")]
        if protocol == ProtocolId::WhatsApp {
            self.end_pairing();
        }
        if self.selected_protocol == protocol {
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
        }
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
    /// Older-message paging is Telegram-only today (#61).
    fn stop_spinners(&mut self, protocol: ProtocolId, chat: Option<&str>) {
        match chat {
            Some(chat) => {
                self.history_loading.remove(&(protocol, chat.to_owned()));
                if protocol == ProtocolId::Telegram {
                    self.older_loading.remove(chat);
                }
            }
            None => {
                self.chat_list_loading.remove(&protocol);
                self.history_loading.retain(|(owner, _)| *owner != protocol);
                if protocol == ProtocolId::Telegram {
                    self.older_loading.clear();
                }
            }
        }
    }

    fn remove_conversation(&mut self, protocol: ProtocolId, id: &str) {
        if let Some(list) = self.conversations.get_mut(&protocol) {
            list.retain(|row| row.id != id);
        }
        self.messages
            .retain(|key, _| !(key.0 == protocol && key.1 == id));
        // Every removed chat loses its draft, selected or not (#43).
        self.drafts.remove(&(protocol, id.to_owned()));
        self.drop_rejected_body(protocol, id);
        if protocol == ProtocolId::Telegram {
            self.older_loading.remove(id);
            self.older_at_start.remove(id);
            self.older_retry.remove(id);
        }
        if self.selected_protocol == protocol && self.selected_conversation.as_deref() == Some(id) {
            self.compose.clear();
            self.selected_conversation = None;
            self.ensure_conversation_selection();
        }
        self.sync_focused_row(FocusFollow::List);
    }

    /// Keep the highlight on a visible row.
    ///
    /// Set `scroll_to_focused` when the highlight moves by key, when the search
    /// text changes, or when the highlight's visible index changes.
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

    fn delivery_of(
        &self,
        protocol: ProtocolId,
        conversation_id: &str,
        id: &str,
    ) -> Option<Delivery> {
        self.messages
            .get(&(protocol, conversation_id.to_string()))?
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
        let Some(message) = self
            .messages
            .get_mut(&(protocol, conversation_id.to_string()))
            .and_then(|list| list.iter_mut().find(|row| row.id == message_id))
        else {
            return;
        };
        message.delivery = delivery;
        let message = message.clone();
        self.note_delivery(before, &message);
    }

    /// A send that was pending and now failed: show the error block and keep the text.
    fn note_delivery(&mut self, before: Option<Delivery>, message: &ChatMessage) {
        if !message.outbound
            || message.delivery != Delivery::Failed
            || before != Some(Delivery::Pending)
        {
            return;
        }
        let selected = self.selected_protocol == message.protocol
            && self.selected_conversation.as_deref() == Some(message.conversation_id.as_str());
        if selected {
            if self.compose.trim().is_empty() {
                self.compose.clone_from(&message.body);
            }
        } else {
            self.drafts
                .entry((message.protocol, message.conversation_id.clone()))
                .or_insert_with(|| message.body.clone());
        }
        self.set_error(
            "Message not sent.",
            "Telegram did not accept the message.",
            "Press Retry on the message, or edit the text and send it again.",
        );
    }

    fn upsert_message(&mut self, message: ChatMessage) {
        let key = (message.protocol, message.conversation_id.clone());
        let list = self.messages.entry(key).or_default();
        if let Some(existing) = list.iter_mut().find(|row| row.id == message.id) {
            *existing = message;
        } else {
            list.push(message);
        }
        sort_messages(list);
    }

    fn remove_message(&mut self, protocol: ProtocolId, conversation_id: &str, message_id: &str) {
        let Some(list) = self
            .messages
            .get_mut(&(protocol, conversation_id.to_string()))
        else {
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
        let Some(list) = self
            .messages
            .get_mut(&(protocol, conversation_id.to_string()))
        else {
            return;
        };
        list.retain(|row| !message_ids.contains(&row.id));
    }

    fn patch_message_body(
        &mut self,
        protocol: ProtocolId,
        conversation_id: &str,
        message_id: &str,
        body: String,
    ) {
        let Some(list) = self
            .messages
            .get_mut(&(protocol, conversation_id.to_string()))
        else {
            return;
        };
        if let Some(message) = list.iter_mut().find(|row| row.id == message_id) {
            message.body = body;
        }
    }

    /// WhatsApp pairing chrome when feature `whatsapp-web` is compiled.
    ///
    /// The entry does not wait for a linked Telegram account. The default
    /// build leaves the feature off, so first-run chrome stays Telegram-only.
    #[cfg(feature = "whatsapp-web")]
    #[must_use]
    pub fn whatsapp_pairing_available(&self) -> bool {
        protocol_chrome_enabled(ProtocolId::WhatsApp)
    }

    #[cfg(feature = "whatsapp-web")]
    #[must_use]
    pub fn whatsapp_gate_open(&self) -> bool {
        self.whatsapp_pairing_available() && !matches!(self.whatsapp_screen, WhatsAppScreen::Hidden)
    }

    #[cfg(feature = "whatsapp-web")]
    pub fn open_whatsapp_risk_gate(&mut self) {
        if !self.whatsapp_pairing_available() {
            return;
        }
        if self.whatsapp_started {
            self.pending.push(AdapterCommand::WhatsAppCancelLink);
        }
        self.whatsapp_screen = WhatsAppScreen::RiskGate;
        self.end_pairing();
        self.whatsapp_started = false;
        self.whatsapp_risk_acknowledged = false;
        self.whatsapp_phone.clear();
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
        phone.clear();
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
        self.status_text = "WhatsApp ban gate accepted. Pairing has not started.".into();
        self.pending.push(AdapterCommand::WhatsAppAcknowledgeRisk);
    }

    #[cfg(feature = "whatsapp-web")]
    pub fn begin_whatsapp_link(&mut self, phone: &WhatsAppPhoneVault) {
        // Pairing starts only from the pair screen, after the gate was accepted.
        if self.whatsapp_screen != WhatsAppScreen::Pair || !self.whatsapp_risk_acknowledged {
            tracing::warn!("whatsapp pairing dropped: the risk gate was not accepted");
            return;
        }
        if self.whatsapp_started {
            return;
        }
        phone.set_phone(&self.whatsapp_phone);
        self.whatsapp_phone.clear();
        self.whatsapp_started = true;
        self.error = None;
        self.status_text = "WhatsApp pairing requested.".into();
        let generation = self.next_pairing_generation;
        self.next_pairing_generation += 1;
        self.pairing_generation = Some(generation);
        self.pending
            .push(AdapterCommand::WhatsAppBeginLink { generation });
    }

    /// WhatsApp linked (`Account { Linked }`): close the pair screen the user
    /// started and drop the pairing material. A reconnect also links; it
    /// must not close a risk gate the user just opened.
    #[cfg(feature = "whatsapp-web")]
    fn finish_whatsapp_link(&mut self) {
        if !self.whatsapp_started || self.whatsapp_screen != WhatsAppScreen::Pair {
            return;
        }
        self.whatsapp_screen = WhatsAppScreen::Hidden;
        self.end_pairing();
        self.whatsapp_phone.clear();
    }

    /// No pairing runs any more: drop the shown QR and pair code, and every
    /// later payload of the old pairing.
    #[cfg(feature = "whatsapp-web")]
    fn end_pairing(&mut self) {
        self.pairing_generation = None;
        self.whatsapp_qr = None;
        self.whatsapp_pair_code = None;
    }

    /// A pairing payload belongs to the pairing that runs now.
    #[cfg(any(feature = "whatsapp-web", feature = "signal-local"))]
    fn current_pairing(&self, generation: u64) -> bool {
        let current = self.pairing_generation == Some(generation);
        if !current {
            tracing::warn!("pairing payload dropped: it is from an older pairing");
        }
        current
    }

    #[cfg(feature = "whatsapp-web")]
    pub fn cancel_whatsapp_link(&mut self, phone: &WhatsAppPhoneVault) {
        phone.clear();
        self.whatsapp_phone.clear();
        self.end_pairing();
        self.whatsapp_started = false;
        self.whatsapp_risk_acknowledged = false;
        self.whatsapp_screen = WhatsAppScreen::Hidden;
        self.status_text = "WhatsApp pairing cancelled.".into();
        self.pending.push(AdapterCommand::WhatsAppCancelLink);
    }

    /// Signal linking chrome when feature `signal-local` is compiled.
    ///
    /// The default build leaves the feature off, so first-run chrome stays
    /// Telegram-only.
    #[cfg(feature = "signal-local")]
    #[must_use]
    pub fn signal_linking_available(&self) -> bool {
        protocol_chrome_enabled(ProtocolId::Signal)
    }

    #[cfg(feature = "signal-local")]
    #[must_use]
    pub fn signal_gate_open(&self) -> bool {
        self.signal_linking_available() && !matches!(self.signal_screen, SignalScreen::Hidden)
    }

    #[cfg(feature = "signal-local")]
    pub fn open_signal_notice(&mut self) {
        if !self.signal_linking_available() {
            return;
        }
        if self.protocol_linked(ProtocolId::Signal) {
            self.signal_screen = SignalScreen::Notice;
            self.signal_qr = None;
            return;
        }
        if self.signal_started {
            self.pending.push(AdapterCommand::SignalCancelLink);
        }
        self.signal_screen = SignalScreen::Notice;
        self.signal_qr = None;
        self.signal_started = false;
        self.signal_notice_acknowledged = false;
    }

    #[cfg(feature = "signal-local")]
    /// Leave the notice or the link screen. After the notice was accepted
    /// this is Cancel: linking stops and the acknowledgement resets. From
    /// the notice alone it only hides the screen.
    pub fn close_signal_gate(&mut self) {
        if self.protocol_linked(ProtocolId::Signal) {
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
        self.signal_notice_acknowledged = true;
        self.signal_screen = SignalScreen::Link;
        self.error = None;
        self.status_text = "Signal local-build notice accepted. Linking has not started.".into();
        self.pending.push(AdapterCommand::SignalAcknowledgeNotice);
    }

    #[cfg(feature = "signal-local")]
    pub fn begin_signal_link(&mut self) {
        // Linking starts only from the link screen, after the notice was accepted.
        if self.signal_screen != SignalScreen::Link || !self.signal_notice_acknowledged {
            tracing::warn!("signal linking dropped: the local-build notice was not accepted");
            return;
        }
        if self.signal_started {
            return;
        }
        self.signal_started = true;
        self.error = None;
        self.status_text = "Signal linking requested.".into();
        let generation = self.next_pairing_generation;
        self.next_pairing_generation += 1;
        self.pairing_generation = Some(generation);
        self.pending
            .push(AdapterCommand::SignalBeginLink { generation });
    }

    /// Leave the link screen after `Account { Linked }`. The worker stays up.
    /// This does not send `SignalCancelLink`.
    #[cfg(feature = "signal-local")]
    pub fn finish_signal_link(&mut self) {
        if self.signal_screen == SignalScreen::Hidden {
            return;
        }
        self.signal_screen = SignalScreen::Hidden;
        self.pairing_generation = None;
        self.signal_qr = None;
        self.status_text = "Signal is linked on this local build.".into();
        self.select_protocol(ProtocolId::Signal);
    }

    #[cfg(feature = "signal-local")]
    pub fn cancel_signal_link(&mut self) {
        self.pairing_generation = None;
        self.signal_qr = None;
        self.signal_started = false;
        self.signal_notice_acknowledged = false;
        self.signal_screen = SignalScreen::Hidden;
        self.status_text = "Signal linking cancelled.".into();
        self.pending.push(AdapterCommand::SignalCancelLink);
    }
}

fn sort_conversations(list: &mut [Conversation]) {
    list.sort_by_key(|row| (std::cmp::Reverse(row.order), row.id.clone()));
}

fn sort_messages(list: &mut [ChatMessage]) {
    // Send time first: some protocols (WhatsApp) use ids with no order.
    // Numeric ids rank the rest when the time is unknown (zero).
    list.sort_by_key(|row| (row.sent_at, message_rank(&row.id)));
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
        }
    }

    /// Telegram signed in, as after Ready: the flag and the linked row.
    pub fn link_telegram(snapshot: &mut Snapshot) {
        snapshot.telegram_authorized = true;
        if let Some(row) = snapshot
            .accounts
            .iter_mut()
            .find(|row| row.caps.id == ProtocolId::Telegram)
        {
            row.state = AccountState::Linked;
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
    fn selected_row_requests_scroll_only_when_it_moves() {
        let store = SecretStore::memory();
        let mut snapshot = Snapshot::new();
        complete_telegram(&mut snapshot, &store);
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: telegram_chat(1, "Ada", 10),
        });
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: telegram_chat(2, "Bob", 5),
        });
        snapshot.select_conversation("telegram:2".into());
        assert!(!snapshot.take_scroll_to_selected());

        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: telegram_chat(1, "Ada", 11),
        });
        assert!(
            !snapshot.take_scroll_to_selected(),
            "selected row did not move"
        );

        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: telegram_chat(3, "Cy", 99),
        });
        assert!(snapshot.take_scroll_to_selected());
        assert!(!snapshot.take_scroll_to_selected(), "one request per move");
    }

    #[test]
    fn a_resort_scrolls_the_highlighted_row_only_when_it_moves() {
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
        assert!(snapshot.take_scroll_to_focused());
        assert!(!snapshot.wants_scroll_to_focused(), "one request per move");
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
        assert!(snapshot.take_scroll_to_focused());
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
        assert!(
            snapshot.take_scroll_to_focused(),
            "a resort that moves the highlight scrolls"
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
        });
        snapshot.take_commands();
        snapshot
    }

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
        assert_eq!(snapshot.unread_total(), 8, "the muted chat does not count");
        assert!(
            snapshot
                .conversation(ProtocolId::Telegram, "telegram:2")
                .is_some_and(|row| row.muted)
        );
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Telegram,
            state: AccountState::Unlinked,
        });
        assert_eq!(snapshot.unread_total(), 0, "no session, no count");
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
        });
        snapshot.take_commands();
        assert_eq!(snapshot.older_state(), OlderState::Idle);
        snapshot.load_older();
        assert_eq!(older_requests(&mut snapshot), vec!["telegram:2:9"]);
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
        assert!(!snapshot.older_at_start.contains("telegram:1"));
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
                .contains_key(&(ProtocolId::Telegram, "telegram:1".into()))
        );

        snapshot.apply(AdapterEvent::ConversationRemoved {
            protocol: ProtocolId::Telegram,
            id: "telegram:1".into(),
        });
        assert!(
            !snapshot
                .drafts
                .contains_key(&(ProtocolId::Telegram, "telegram:1".into())),
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
        let filters = InboxFilter::chrome_filters();
        assert!(filters.contains(&InboxFilter::All));
        assert!(filters.contains(&InboxFilter::Telegram));
        #[cfg(feature = "slack-oauth")]
        assert!(filters.contains(&InboxFilter::Slack));
        #[cfg(not(feature = "slack-oauth"))]
        assert_eq!(filters.len(), 2);
        assert!(
            !filters
                .iter()
                .any(|filter| filter.label() == "Experimental")
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

    #[test]
    fn filter_matches_supported_protocols_without_an_experimental_tab() {
        assert!(InboxFilter::Telegram.matches(ProtocolId::Telegram));
        assert!(!InboxFilter::Telegram.matches(ProtocolId::Slack));
        #[cfg(feature = "slack-oauth")]
        assert!(InboxFilter::Slack.matches(ProtocolId::Slack));
        assert!(InboxFilter::All.matches(ProtocolId::Discord));
        assert!(InboxFilter::All.matches(ProtocolId::WhatsApp));
        assert!(
            !InboxFilter::chrome_filters()
                .iter()
                .any(|filter| filter.label() == "Experimental")
        );
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
    fn off_feature_spikes_stay_out_of_the_switcher() {
        for filter in InboxFilter::chrome_filters() {
            let mut view = Snapshot::new();
            view.set_filter(*filter);
            assert_eq!(
                view.shows_in_switcher(ProtocolId::WhatsApp),
                cfg!(feature = "whatsapp-web") && filter.matches(ProtocolId::WhatsApp)
            );
            assert_eq!(
                view.shows_in_switcher(ProtocolId::Discord),
                DiscordAdapter::bot_inbox_compiled() && filter.matches(ProtocolId::Discord)
            );
            assert_eq!(
                view.shows_in_switcher(ProtocolId::Slack),
                cfg!(feature = "slack-oauth") && filter.matches(ProtocolId::Slack)
            );
            assert_eq!(
                view.shows_in_switcher(ProtocolId::Signal),
                cfg!(feature = "signal-local") && filter.matches(ProtocolId::Signal)
            );
            assert_eq!(
                view.shows_in_switcher(ProtocolId::Telegram),
                filter.matches(ProtocolId::Telegram)
            );
        }
        assert!(!InboxFilter::Telegram.shows_in_switcher(ProtocolId::Slack));
        #[cfg(feature = "slack-oauth")]
        assert!(!InboxFilter::Slack.shows_in_switcher(ProtocolId::Telegram));
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
        snapshot.set_draft(ProtocolId::Telegram, "telegram:2", "other chat".into());
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
            (ProtocolId::Telegram, "telegram:2".into()),
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
        snapshot.set_draft(ProtocolId::Discord, "discord:9:9", "other chat".into());
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
                .get(&(ProtocolId::Discord, "discord:1:2".into()))
                .map(String::as_str),
            Some("keep me"),
        );
        assert!(
            !snapshot
                .drafts
                .contains_key(&(ProtocolId::Discord, "discord:9:9".into())),
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
        snapshot.set_draft(ProtocolId::Telegram, "telegram:2", "telegram draft".into());
        snapshot.set_draft(ProtocolId::Slack, "telegram:2", "slack draft".into());
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
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Slack,
            state: AccountState::Linking,
        });
        link(&mut snapshot, ProtocolId::Slack);
        assert!(open_chats(&mut snapshot).is_empty(), "no reload");
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
        let key = (ProtocolId::Telegram, "telegram:1".to_owned());
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
        snapshot.older_loading.insert("telegram:1".into());
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

    // endregion: #80
}
