//! UI-side snapshot. Mutated only on the UI thread from polled events and clicks.

use std::collections::{HashMap, HashSet};

#[cfg(feature = "whatsapp-web")]
use thinwire_protocol::WhatsAppPhoneVault;
use thinwire_protocol::{
    AdapterCommand, AdapterEvent, AdapterStatus, ChatMessage, Conversation, DiscordAdapter,
    ProtocolCapabilities, ProtocolId, TelegramApiSource, TelegramAuthPhase, TelegramAuthStep,
    TelegramSecretVault, catalog, parse_telegram_chat_id, telegram_api_available,
};

use super::secrets::{SecretKey, SecretStore};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InboxFilter {
    All,
    Telegram,
    #[cfg(feature = "slack-oauth")]
    Slack,
}

impl InboxFilter {
    /// Top-bar filters for this build. Spike tabs stay out until their feature is on.
    #[must_use]
    pub(crate) fn chrome_filters() -> &'static [Self] {
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
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Telegram => "Telegram",
            #[cfg(feature = "slack-oauth")]
            Self::Slack => "Slack",
        }
    }

    #[must_use]
    pub(crate) const fn matches(self, protocol: ProtocolId) -> bool {
        match self {
            Self::All => true,
            Self::Telegram => matches!(protocol, ProtocolId::Telegram),
            #[cfg(feature = "slack-oauth")]
            Self::Slack => matches!(protocol, ProtocolId::Slack),
        }
    }

    /// Account chip visibility for the current filter. Off-feature spikes stay out.
    ///
    /// Discord also needs live Telegram messages before it may appear — that gate
    /// lives on [`Snapshot::account_surface_visible`], so callers must AND both.
    #[must_use]
    pub(crate) const fn shows_in_switcher(self, protocol: ProtocolId) -> bool {
        self.matches(protocol)
    }
}

/// Compile-time chrome gate: spikes stay invisible when their feature is off.
#[must_use]
pub(crate) const fn protocol_chrome_enabled(protocol: ProtocolId) -> bool {
    match protocol {
        ProtocolId::Telegram => true,
        ProtocolId::Slack => cfg!(feature = "slack-oauth"),
        ProtocolId::WhatsApp => cfg!(feature = "whatsapp-web"),
        ProtocolId::Discord => cfg!(feature = "discord-bot"),
    }
}

/// Non-modal Telegram login steps. Credential field values never leave this snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthScreen {
    Idle,
    NeedCredentials,
    TelegramApi,
    TelegramPhone,
    TelegramCode,
    Telegram2fa,
}

/// Start-up resume of a saved Telegram session. Runs once per launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Resume {
    /// The OS keychain has not finished its first read.
    Waiting,
    /// A saved session exists. TDLib is starting with no click.
    Connecting,
    /// Resume finished, failed, or did not apply.
    Settled,
}

/// What the center panel shows. Pure state, so tests do not need egui.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CenterView {
    Auth,
    /// Spinner. `true` adds the "Connecting to Telegram…" text.
    Resuming {
        connecting: bool,
    },
    FirstRun,
    Thread,
}

/// Copy on the phone step when a saved session no longer works.
pub(crate) const SESSION_ENDED_NOTICE: &str = "Your Telegram session ended. Sign in again.";

/// Center panel copy while a saved session reconnects.
pub(crate) const RESUME_CONNECTING: &str = "Connecting to Telegram…";

/// Experimental WhatsApp screens. Only the `whatsapp-web` build can enter them.
#[cfg(feature = "whatsapp-web")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WhatsAppScreen {
    Hidden,
    RiskGate,
    Pair,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UserError {
    pub happened: String,
    pub why: String,
    pub next: String,
}

#[derive(Debug, Clone)]
pub(crate) struct AccountRow {
    pub caps: ProtocolCapabilities,
    pub status: AdapterStatus,
    pub detail: String,
    pub linked: bool,
}

#[derive(Debug)]
pub(crate) struct Snapshot {
    pub accounts: Vec<AccountRow>,
    conversations: HashMap<ProtocolId, Vec<Conversation>>,
    messages: HashMap<(ProtocolId, String), Vec<ChatMessage>>,
    pub selected_protocol: ProtocolId,
    pub selected_conversation: Option<String>,
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
    pub compose: String,
    pub auth_busy: bool,
    /// One line above the active login form. Never holds a secret.
    pub auth_notice: Option<&'static str>,
    pub telegram_authorized: bool,
    resume: Resume,
    telegram_messages_from_adapter: u32,
    api_source: TelegramApiSource,
    pending: Vec<AdapterCommand>,
    keychain_flush: bool,
    #[cfg(feature = "whatsapp-web")]
    pub(crate) whatsapp_screen: WhatsAppScreen,
    #[cfg(feature = "whatsapp-web")]
    pub(crate) whatsapp_phone: String,
    #[cfg(feature = "whatsapp-web")]
    pub(crate) whatsapp_qr: Option<String>,
    #[cfg(feature = "whatsapp-web")]
    pub(crate) whatsapp_pair_code: Option<String>,
    #[cfg(feature = "whatsapp-web")]
    pub(crate) whatsapp_started: bool,
}

impl Snapshot {
    pub(crate) fn new() -> Self {
        let accounts = catalog()
            .into_iter()
            .map(|caps| AccountRow {
                caps,
                status: AdapterStatus::Stubbed,
                detail: caps.detail.to_string(),
                linked: false,
            })
            .collect();
        Self {
            accounts,
            conversations: HashMap::new(),
            messages: HashMap::new(),
            selected_protocol: ProtocolId::Telegram,
            selected_conversation: None,
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
            compose: String::new(),
            auth_busy: false,
            auth_notice: None,
            telegram_authorized: false,
            resume: Resume::Waiting,
            telegram_messages_from_adapter: 0,
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
        }
    }

    #[cfg(test)]
    pub(crate) fn with_api_source(source: TelegramApiSource) -> Self {
        let mut snapshot = Self::new();
        snapshot.api_source = source;
        snapshot
    }

    pub(crate) fn apply(&mut self, event: AdapterEvent) {
        match event {
            AdapterEvent::Status {
                protocol,
                status,
                detail,
            } => {
                if let Some(row) = self.accounts.iter_mut().find(|row| row.caps.id == protocol) {
                    row.status = status;
                    row.detail = detail.clone();
                    if protocol == ProtocolId::Discord {
                        row.linked = DiscordAdapter::inbox_account_linked(status, &detail);
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
                {
                    let list = self.conversations.entry(protocol).or_default();
                    if let Some(existing) = list.iter_mut().find(|row| row.id == conversation.id) {
                        *existing = conversation;
                    } else {
                        list.push(conversation);
                    }
                    sort_conversations(list);
                }
                self.ensure_conversation_selection();
            }
            AdapterEvent::ConversationRemoved { protocol, id } => {
                self.remove_conversation(protocol, &id);
            }
            AdapterEvent::MessageReceived { message } => {
                if message.protocol == ProtocolId::Telegram {
                    self.telegram_messages_from_adapter =
                        self.telegram_messages_from_adapter.saturating_add(1);
                }
                self.upsert_message(message);
            }
            AdapterEvent::MessageReplaced {
                protocol,
                conversation_id,
                old_id,
                message,
            } => {
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
            AdapterEvent::FlushSecrets => {
                self.keychain_flush = true;
            }
            AdapterEvent::WhatsAppQr {
                code,
                generation: _,
            } => {
                #[cfg(feature = "whatsapp-web")]
                {
                    self.whatsapp_qr = Some(code.reveal().to_string());
                }
                #[cfg(not(feature = "whatsapp-web"))]
                {
                    let _ = code;
                }
            }
            AdapterEvent::WhatsAppPairCode {
                code,
                generation: _,
            } => {
                #[cfg(feature = "whatsapp-web")]
                {
                    self.whatsapp_pair_code = Some(code.reveal().to_string());
                }
                #[cfg(not(feature = "whatsapp-web"))]
                {
                    let _ = code;
                }
            }
        }
    }

    pub(crate) fn take_commands(&mut self) -> Vec<AdapterCommand> {
        std::mem::take(&mut self.pending)
    }

    #[must_use]
    pub(crate) fn take_keychain_flush(&mut self) -> bool {
        std::mem::take(&mut self.keychain_flush)
    }

    /// Start TDLib with no click when the keychain holds a saved session.
    ///
    /// Call once per frame. It acts only after the keychain read settles, and
    /// only once. The UI thread reads memory only; the command has no secret.
    pub(crate) fn poll_resume(&mut self, store: &SecretStore) {
        self.try_resume(store, super::auth::tdlib_compiled());
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
    pub(crate) fn center_view(&self) -> CenterView {
        if self.auth != AuthScreen::Idle {
            return CenterView::Auth;
        }
        if self.has_primary_account() {
            return CenterView::Thread;
        }
        match self.resume {
            Resume::Waiting if super::auth::tdlib_compiled() => {
                CenterView::Resuming { connecting: false }
            }
            Resume::Connecting => CenterView::Resuming { connecting: true },
            Resume::Waiting | Resume::Settled => CenterView::FirstRun,
        }
    }

    pub(crate) fn has_primary_account(&self) -> bool {
        self.accounts
            .iter()
            .any(|row| row.linked && matches!(row.caps.id, ProtocolId::Telegram))
    }

    pub(crate) fn select_protocol(&mut self, protocol: ProtocolId) {
        if !self.account_surface_visible(protocol) {
            return;
        }
        self.selected_protocol = protocol;
        self.selected_conversation = None;
        self.ensure_conversation_selection();
    }

    pub(crate) fn select_conversation(&mut self, id: String) {
        self.selected_conversation = Some(id);
        self.queue_open_chat();
    }

    pub(crate) fn set_filter(&mut self, filter: InboxFilter) {
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
    }

    /// Discord stays hidden until the bot feature is compiled and Telegram has messages.
    #[must_use]
    pub(crate) fn discord_inbox_visible(&self) -> bool {
        DiscordAdapter::bot_inbox_compiled()
            && self.telegram_authorized
            && self.telegram_messages_from_adapter > 0
    }

    /// Protocols that may appear in Accounts / filter chrome for this build.
    ///
    /// Telegram is always present. Slack and WhatsApp appear only when their
    /// cargo features are on. Discord also waits for Telegram messages (0009).
    #[must_use]
    pub(crate) fn account_surface_visible(&self, protocol: ProtocolId) -> bool {
        match protocol {
            ProtocolId::Discord => self.discord_inbox_visible(),
            other => protocol_chrome_enabled(other),
        }
    }

    #[must_use]
    pub(crate) fn shows_in_switcher(&self, protocol: ProtocolId) -> bool {
        self.filter.shows_in_switcher(protocol) && self.account_surface_visible(protocol)
    }

    pub(crate) fn visible_conversations(&self) -> Vec<&Conversation> {
        if !self.account_surface_visible(self.selected_protocol)
            || !self.protocol_linked(self.selected_protocol)
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
    pub(crate) fn unread_for(&self, protocol: ProtocolId) -> u32 {
        if !self.protocol_linked(protocol) {
            return 0;
        }
        self.conversations
            .get(&protocol)
            .map(|rows| rows.iter().map(|row| row.unread).sum())
            .unwrap_or(0)
    }

    fn protocol_linked(&self, protocol: ProtocolId) -> bool {
        self.accounts
            .iter()
            .any(|row| row.caps.id == protocol && row.linked)
    }

    pub(crate) fn selected_conversation_row(&self) -> Option<&Conversation> {
        let id = self.selected_conversation.as_ref()?;
        self.conversations
            .get(&self.selected_protocol)?
            .iter()
            .find(|row| row.id == *id)
    }

    pub(crate) fn selected_messages(&self) -> &[ChatMessage] {
        let Some(id) = self.selected_conversation.as_ref() else {
            return &[];
        };
        self.messages
            .get(&(self.selected_protocol, id.clone()))
            .map_or(&[], Vec::as_slice)
    }

    pub(crate) fn refresh_visible(&mut self) {
        let protocols: HashSet<ProtocolId> = self
            .accounts
            .iter()
            .map(|row| row.caps.id)
            .filter(|id| self.filter.matches(*id) && self.account_surface_visible(*id))
            .collect();
        for protocol in protocols {
            if protocol == ProtocolId::Telegram && self.telegram_authorized {
                self.pending.push(AdapterCommand::LoadChats { protocol });
            } else {
                self.pending.push(AdapterCommand::Connect { protocol });
            }
        }
        self.status_text = "Refreshing…".into();
    }

    #[must_use]
    pub(crate) fn has_api_credentials(&self, store: &SecretStore) -> bool {
        telegram_api_available(store, &self.api_source)
    }

    #[must_use]
    pub(crate) fn telegram_ready(&self) -> bool {
        self.telegram_authorized
    }

    pub(crate) fn open_add_account(&mut self, store: &SecretStore) {
        self.open_telegram(store);
    }

    pub(crate) fn cancel_auth(&mut self, store: &SecretStore) {
        self.clear_secrets();
        self.resume = Resume::Settled;
        clear_ephemeral(store);
        self.auth = AuthScreen::Idle;
        self.auth_busy = false;
        self.telegram_authorized = false;
        self.error = None;
        self.status_text = "Account linking cancelled.".into();
        self.pending.push(AdapterCommand::Disconnect {
            protocol: ProtocolId::Telegram,
        });
    }

    pub(crate) fn advance_telegram(&mut self, store: &SecretStore) {
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
                store.set_secret(SecretKey::Password, &self.telegram_2fa);
                self.queue_telegram_step(TelegramAuthStep::TwoFactor);
                self.mark_auth_busy("Telegram: optional 2FA submitted. Waiting for authorization.");
            }
            AuthScreen::NeedCredentials | AuthScreen::Idle => {}
        }
    }

    pub(crate) fn send_compose(&mut self) {
        let Some(conversation_id) = self.selected_conversation.clone() else {
            self.set_error(
                "Nothing was sent.",
                "No conversation is selected.",
                "Pick a thread in the inbox, then type in the compose field.",
            );
            return;
        };
        let body = self.compose.trim().to_string();
        if body.is_empty() {
            self.set_error(
                "Nothing was sent.",
                "The compose field is empty.",
                "Type a message for the selected Telegram chat.",
            );
            return;
        }
        if self.selected_protocol != ProtocolId::Telegram || !self.telegram_authorized {
            self.set_error(
                "Nothing was sent.",
                "Telegram is not ready.",
                "Sign in with Telegram, then pick a chat.",
            );
            return;
        }
        if parse_telegram_chat_id(&conversation_id).is_none() {
            self.set_error(
                "Nothing was sent.",
                "That chat is not a Telegram chat id.",
                "Pick a chat from the Telegram list.",
            );
            return;
        }
        self.compose.clear();
        self.error = None;
        self.pending.push(AdapterCommand::SendText {
            protocol: ProtocolId::Telegram,
            conversation_id,
            body,
        });
        self.status_text = "Sending…".into();
    }

    pub(crate) fn open_telegram(&mut self, store: &SecretStore) {
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

    pub(crate) fn open_api_override(&mut self, store: &SecretStore) {
        self.clear_secrets();
        self.error = None;
        self.auth_busy = false;
        self.prefill_from_store(store);
        self.auth = AuthScreen::TelegramApi;
        self.status_text =
            "Advanced: custom Telegram API credentials. The keychain override wins over the publisher pair. Values are not logged.".into();
    }

    fn start_phone_login(&mut self) {
        self.auth = AuthScreen::TelegramPhone;
        self.queue_telegram_step(TelegramAuthStep::ApiCredentials);
        self.mark_auth_busy("Telegram: using stored or publisher API credentials. Enter a phone number when the next step is ready.");
    }

    fn apply_telegram_phase(&mut self, phase: TelegramAuthPhase) {
        self.auth_busy = false;
        if phase != TelegramAuthPhase::Failed {
            self.error = None;
        }
        let resuming = std::mem::replace(&mut self.resume, Resume::Settled) == Resume::Connecting;
        self.auth_notice = None;
        match phase {
            TelegramAuthPhase::NeedPhone if resuming => {
                // The worker drops the stale session marker on this path.
                self.auth = AuthScreen::TelegramPhone;
                self.auth_notice = Some(SESSION_ENDED_NOTICE);
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
                self.status_text =
                    "Telegram: optional 2FA. Leave blank to skip if this account has none.".into();
            }
            TelegramAuthPhase::Ready => self.finish_telegram_ready(),
            TelegramAuthPhase::Unavailable => self.finish_telegram_unavailable(),
            TelegramAuthPhase::Failed => {
                self.set_error(
                    "Telegram login did not advance.",
                    "The adapter rejected this step.",
                    "Correct the field, or press Cancel. Values are not logged.",
                );
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
            row.linked = true;
        }
        self.telegram_authorized = true;
        self.select_protocol(ProtocolId::Telegram);
        self.auth = AuthScreen::Idle;
        self.auth_busy = false;
        self.error = None;
        self.status_text = "Telegram is ready. Loading the chat list.".into();
    }

    fn finish_telegram_unavailable(&mut self) {
        self.clear_secrets();
        if let Some(row) = self
            .accounts
            .iter_mut()
            .find(|row| row.caps.id == ProtocolId::Telegram)
        {
            row.linked = false;
        }
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
        self.pending.push(AdapterCommand::TelegramAuth { step });
    }

    fn mark_auth_busy(&mut self, status: &str) {
        self.auth_busy = true;
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
        if let Some(first) = self
            .conversations
            .get(&self.selected_protocol)
            .and_then(|rows| rows.first())
        {
            self.selected_conversation = Some(first.id.clone());
            self.queue_open_chat();
        }
    }

    fn queue_open_chat(&mut self) {
        if self.selected_protocol != ProtocolId::Telegram || !self.telegram_authorized {
            return;
        }
        let Some(id) = self.selected_conversation.clone() else {
            return;
        };
        if parse_telegram_chat_id(&id).is_none() {
            return;
        }
        let already = self.pending.iter().any(|command| {
            matches!(
                command,
                AdapterCommand::OpenChat { conversation_id, .. } if conversation_id == &id
            )
        });
        if already {
            return;
        }
        self.pending.push(AdapterCommand::OpenChat {
            protocol: ProtocolId::Telegram,
            conversation_id: id,
        });
    }

    fn remove_conversation(&mut self, protocol: ProtocolId, id: &str) {
        if let Some(list) = self.conversations.get_mut(&protocol) {
            list.retain(|row| row.id != id);
        }
        self.messages
            .retain(|key, _| !(key.0 == protocol && key.1 == id));
        if self.selected_protocol == protocol && self.selected_conversation.as_deref() == Some(id) {
            self.selected_conversation = None;
            self.ensure_conversation_selection();
        }
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

    /// Same predicate as the Telegram-only first-run screen.
    ///
    /// Discord has no pairing control on that screen. The WhatsApp entry stays
    /// unavailable until Telegram is linked, which happens on the Ready path.
    #[cfg(feature = "whatsapp-web")]
    #[must_use]
    pub(crate) fn whatsapp_pairing_available(&self) -> bool {
        self.has_primary_account()
    }

    #[cfg(feature = "whatsapp-web")]
    #[must_use]
    pub(crate) fn whatsapp_gate_open(&self) -> bool {
        self.whatsapp_pairing_available() && !matches!(self.whatsapp_screen, WhatsAppScreen::Hidden)
    }

    #[cfg(feature = "whatsapp-web")]
    pub(crate) fn open_whatsapp_risk_gate(&mut self) {
        if !self.whatsapp_pairing_available() {
            return;
        }
        if self.whatsapp_started {
            self.pending.push(AdapterCommand::WhatsAppCancelLink);
        }
        self.whatsapp_screen = WhatsAppScreen::RiskGate;
        self.whatsapp_qr = None;
        self.whatsapp_pair_code = None;
        self.whatsapp_started = false;
        self.whatsapp_phone.clear();
    }

    #[cfg(feature = "whatsapp-web")]
    pub(crate) fn close_whatsapp_gate(&mut self) {
        self.whatsapp_screen = WhatsAppScreen::Hidden;
        self.whatsapp_phone.clear();
    }

    #[cfg(feature = "whatsapp-web")]
    pub(crate) fn acknowledge_whatsapp_risk(&mut self) {
        self.whatsapp_screen = WhatsAppScreen::Pair;
        self.error = None;
        self.status_text = "WhatsApp ban gate accepted. Pairing has not started.".into();
        self.pending.push(AdapterCommand::WhatsAppAcknowledgeRisk);
    }

    #[cfg(feature = "whatsapp-web")]
    pub(crate) fn begin_whatsapp_link(&mut self, phone: &WhatsAppPhoneVault) {
        if self.whatsapp_started {
            return;
        }
        phone.set_phone(&self.whatsapp_phone);
        self.whatsapp_phone.clear();
        self.whatsapp_started = true;
        self.error = None;
        self.status_text = "WhatsApp pairing requested.".into();
        self.pending.push(AdapterCommand::WhatsAppBeginLink);
    }

    #[cfg(feature = "whatsapp-web")]
    pub(crate) fn cancel_whatsapp_link(&mut self, phone: &WhatsAppPhoneVault) {
        phone.clear();
        self.whatsapp_phone.clear();
        self.whatsapp_qr = None;
        self.whatsapp_pair_code = None;
        self.whatsapp_started = false;
        self.whatsapp_screen = WhatsAppScreen::Hidden;
        self.status_text = "WhatsApp pairing cancelled.".into();
        self.pending.push(AdapterCommand::WhatsAppCancelLink);
    }
}

fn sort_conversations(list: &mut [Conversation]) {
    list.sort_by_key(|row| (std::cmp::Reverse(row.order), row.id.clone()));
}

fn sort_messages(list: &mut [ChatMessage]) {
    list.sort_by_key(|row| message_rank(&row.id));
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
) -> Result<(), super::secrets::SecretError> {
    store.set(SecretKey::ApiId, api_id)?;
    store.set(SecretKey::ApiHash, api_hash)?;
    Ok(())
}

fn clear_ephemeral(store: &SecretStore) {
    for key in SecretKey::EPHEMERAL {
        store.set_secret(key, "");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn submit_and_apply(snapshot: &mut Snapshot, store: &SecretStore, phase: TelegramAuthPhase) {
        snapshot.advance_telegram(store);
        snapshot.apply(AdapterEvent::TelegramAuth { phase });
    }

    fn seed_override(store: &SecretStore) {
        store.set(SecretKey::ApiId, "11111").expect("id");
        store.set(SecretKey::ApiHash, "hash-value").expect("hash");
    }

    fn complete_telegram(snapshot: &mut Snapshot, store: &SecretStore) {
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
        submit_and_apply(snapshot, store, TelegramAuthPhase::Ready);
        assert_eq!(snapshot.auth, AuthScreen::Idle);
        assert!(snapshot.telegram_ready());
    }

    fn resume_commands(snapshot: &mut Snapshot) -> usize {
        snapshot
            .take_commands()
            .iter()
            .filter(|command| {
                matches!(
                    command,
                    AdapterCommand::TelegramAuth {
                        step: TelegramAuthStep::ApiCredentials
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
                step: TelegramAuthStep::ApiCredentials
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
        assert_eq!(snapshot.auth_notice, Some(SESSION_ENDED_NOTICE));
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

    #[test]
    fn auth_ui_is_telegram_only_this_beat() {
        let src = include_str!("auth.rs");
        assert!(src.contains("TELEGRAM_API_ID"));
        assert!(src.contains("credentials missing") || src.contains("Credentials missing"));
        assert!(src.contains("my.telegram.org"));
        assert!(src.contains("Cancel"));
        assert!(src.contains("Send code"));
        assert!(!src.contains("Continue (stub)"));
        assert!(!src.contains("Send code (stub)"));
        assert!(!src.contains("Finish (stub)"));
        assert!(!src.contains("WhatsApp"));
        assert!(!src.contains("Discord"));
        assert!(!src.contains("Slack"));
        assert!(!src.contains("UserAccount"));
        assert!(!src.contains("user_token"));
        let ui = include_str!("ui.rs");
        assert!(ui.contains("not ready"));
        assert!(ui.contains("Start with Telegram"));
        assert!(!ui.contains("not login peers"));
        assert!(!ui.contains("Experimental chips stay visible"));
        assert!(!ui.contains("tokio worker"));
        assert!(!ui.contains("Supported goals:"));
        assert!(!ui.contains("Telegram is live"));
        assert!(!ui.contains("caps.detail"));
        assert!(!ui.contains("account.caps.short_label"));
        assert!(!ui.contains("\"Experimental\""));
        assert!(!ui.contains("my.telegram.org"));
        assert!(src.contains(super::super::auth::TDLIB_UNAVAILABLE_BANNER));
        assert!(ui.contains("stub_banner"));
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
        assert!(!snapshot.shows_in_switcher(ProtocolId::Discord));
        assert_eq!(
            snapshot.shows_in_switcher(ProtocolId::Slack),
            cfg!(feature = "slack-oauth")
        );
    }

    #[test]
    fn first_run_without_credentials_does_not_open_api_screens() {
        let store = SecretStore::memory();
        let mut snapshot = Snapshot::new();
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
        let six = include_str!("../../../../decisions/0006-live-tdlib.md");
        assert!(six.contains("# Live TDLib replaces the Telegram stub"));
        assert!(six.contains("authorizationStateReady"));
        assert!(six.contains("0007-publisher-telegram-api-credentials"));
        let seven =
            include_str!("../../../../decisions/0007-publisher-telegram-api-credentials.md");
        assert!(seven.contains("# Publisher-owned Telegram api_id / api_hash"));
        assert!(seven.contains("Primary login UI: phone/code"));
        assert!(seven.contains("not** the primary login path"));
        assert!(seven.contains("do **not** send every user to my.telegram.org"));
        assert!(seven.contains("GitHub Actions repository secrets"));
        assert!(seven.contains("TELEGRAM_API_ID"));
        assert!(seven.contains("win."));
        assert!(seven.contains("Encrypted inventory copy"));
        assert!(seven.contains("Terraform/SOPS"));
        let roadmap = include_str!("../../../../ROADMAP.md");
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
        assert_eq!(snapshot.auth, AuthScreen::TelegramPhone);
        assert!(snapshot.auth_busy);
        assert!(!snapshot.telegram_ready());
        let commands = snapshot.take_commands();
        assert!(commands.iter().any(|c| matches!(
            c,
            AdapterCommand::TelegramAuth {
                step: TelegramAuthStep::ApiCredentials
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
        assert_eq!(snapshot.auth, AuthScreen::TelegramPhone);
        assert!(snapshot.has_api_credentials(&store));
    }

    #[test]
    fn telegram_auth_waits_for_adapter_phase_events() {
        let store = SecretStore::memory();
        seed_override(&store);
        let mut snapshot = Snapshot::new();
        snapshot.open_telegram(&store);
        assert_eq!(snapshot.auth, AuthScreen::TelegramPhone);
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
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: Conversation {
                protocol: ProtocolId::Telegram,
                id: "telegram:saved".into(),
                title: "Saved Messages".into(),
                participant: "you".into(),
                preview: "secret-preview-should-not-match-search".into(),
                unread: 2,
                order: 0,
            },
        });
        assert!(snapshot.visible_conversations().is_empty());
        assert_eq!(snapshot.unread_for(ProtocolId::Telegram), 0);
        complete_telegram(&mut snapshot, &store);
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
                step: TelegramAuthStep::ApiCredentials
            }
        )));
        assert!(commands.iter().any(|c| matches!(
            c,
            AdapterCommand::TelegramAuth {
                step: TelegramAuthStep::TwoFactor
            }
        )));
        assert!(!commands.iter().any(|c| matches!(
            c,
            AdapterCommand::TelegramAuth {
                step: TelegramAuthStep::Complete
            }
        )));
        let debug = format!("{commands:?}");
        assert!(!debug.contains("11111"));
        assert!(!debug.contains("hash-value"));
        assert!(!debug.contains("+15551234567"));
        assert!(!debug.contains("12345"));
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
        assert_eq!(snapshot.auth, AuthScreen::TelegramPhone);
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
        }
    }

    fn discord_linked(snapshot: &Snapshot) -> bool {
        snapshot
            .accounts
            .iter()
            .find(|row| row.caps.id == ProtocolId::Discord)
            .expect("discord account")
            .linked
    }

    fn unlock_telegram_messages(snapshot: &mut Snapshot) {
        snapshot.apply(AdapterEvent::TelegramAuth {
            phase: TelegramAuthPhase::Ready,
        });
        snapshot.apply(AdapterEvent::MessageReceived {
            message: ChatMessage {
                protocol: ProtocolId::Telegram,
                conversation_id: "telegram:saved".into(),
                id: "telegram:saved:1".into(),
                sender: "worker".into(),
                body: "hello from telegram".into(),
                outbound: false,
            },
        });
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
        unlock_telegram_messages(&mut snapshot);
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
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: discord_guild_placeholder(),
        });
        assert_eq!(
            discord_linked(&snapshot),
            DiscordAdapter::bot_inbox_compiled()
        );
        unlock_telegram_messages(&mut snapshot);
        snapshot.select_protocol(ProtocolId::Discord);
        if DiscordAdapter::bot_inbox_compiled() {
            assert_eq!(snapshot.selected_protocol, ProtocolId::Discord);
            assert_eq!(snapshot.visible_conversations().len(), 1);
            assert_eq!(snapshot.unread_for(ProtocolId::Discord), 1);
        } else {
            assert_eq!(snapshot.selected_protocol, ProtocolId::Telegram);
            assert!(snapshot.visible_conversations().is_empty());
            assert!(!discord_linked(&snapshot));
        }
        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Discord,
            status: AdapterStatus::Refused,
            detail: "Discord user-account tokens are refused.".into(),
        });
        assert!(!discord_linked(&snapshot));
        assert_eq!(snapshot.unread_for(ProtocolId::Discord), 0);
    }

    #[test]
    fn discord_stays_invisible_until_telegram_messages_exist() {
        let mut snapshot = Snapshot::new();
        assert!(!snapshot.discord_inbox_visible());
        assert!(!snapshot.account_surface_visible(ProtocolId::Discord));
        assert_eq!(
            snapshot.account_surface_visible(ProtocolId::WhatsApp),
            cfg!(feature = "whatsapp-web")
        );
        snapshot.apply(AdapterEvent::TelegramAuth {
            phase: TelegramAuthPhase::Ready,
        });
        assert!(snapshot.telegram_ready());
        assert!(!snapshot.discord_inbox_visible());
        snapshot.selected_conversation = Some("telegram:1".into());
        snapshot.compose = "local only".into();
        snapshot.send_compose();
        assert!(!snapshot.discord_inbox_visible());
        snapshot.apply(AdapterEvent::MessageReceived {
            message: ChatMessage {
                protocol: ProtocolId::Telegram,
                conversation_id: "telegram:1".into(),
                id: "telegram:1:1".into(),
                sender: "worker".into(),
                body: "hello from telegram".into(),
                outbound: false,
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
            assert!(
                !view.shows_in_switcher(ProtocolId::Discord),
                "discord needs telegram messages before chrome"
            );
            assert_eq!(
                view.shows_in_switcher(ProtocolId::Slack),
                cfg!(feature = "slack-oauth") && filter.matches(ProtocolId::Slack)
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
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: Conversation {
                protocol: ProtocolId::Telegram,
                id: "telegram:saved".into(),
                title: "Saved Messages".into(),
                participant: "you".into(),
                preview: "secret-preview-should-not-match-search".into(),
                unread: 0,
                order: 0,
            },
        });
        snapshot
            .accounts
            .iter_mut()
            .find(|row| row.caps.id == ProtocolId::Telegram)
            .expect("telegram row")
            .linked = true;
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
        assert!(!row.linked);
        assert!(!snapshot.status_text.contains("qr-do-not-log"));
        assert!(snapshot.take_commands().is_empty());
        snapshot.select_protocol(ProtocolId::WhatsApp);
        assert!(
            !snapshot
                .take_commands()
                .iter()
                .any(|command| matches!(command, AdapterCommand::WhatsAppBeginLink))
        );
    }

    #[test]
    fn whatsapp_spike_ui_is_feature_gated() {
        let app = include_str!("mod.rs");
        assert!(app.contains("#[cfg(feature = \"whatsapp-web\")]\nmod whatsapp_gate;"));
        let ui = include_str!("ui.rs");
        assert!(ui.contains("not ready"));
        assert!(ui.contains("whatsapp_pairing_available"));
        assert!(ui.contains("whatsapp_gate::risk_entry"));
        let gate = include_str!("whatsapp_gate.rs");
        assert!(gate.contains("CRITIC_RISK_BULLETS"));
        assert!(gate.contains("Review WhatsApp ban risk"));
        assert!(gate.contains("No QR code and no pair code are shown on this screen."));
        assert!(
            !gate
                .split(|ch: char| !ch.is_ascii_alphabetic())
                .any(|word| word.eq_ignore_ascii_case("reliable"))
        );
        let ci = include_str!("../../../../.github/workflows/ci.yml");
        assert!(!ci.contains("whatsapp-web"));
        let os_zips = include_str!("../../../../.github/workflows/os-zips.yml");
        assert!(!os_zips.contains("whatsapp-web"));
        let test_sh = include_str!("../../../../scripts/test.sh");
        assert!(!test_sh.contains("whatsapp-web"));
    }

    #[cfg(feature = "whatsapp-web")]
    #[test]
    fn whatsapp_pairing_entry_hidden_until_telegram_first_run() {
        let mut snapshot = Snapshot::new();
        assert!(!snapshot.has_primary_account());
        assert!(!snapshot.telegram_ready());
        assert!(!snapshot.whatsapp_pairing_available());
        snapshot.open_whatsapp_risk_gate();
        assert_eq!(snapshot.whatsapp_screen, WhatsAppScreen::Hidden);
        assert!(!snapshot.whatsapp_gate_open());
        assert!(snapshot.take_commands().is_empty());

        snapshot.apply(AdapterEvent::TelegramAuth {
            phase: TelegramAuthPhase::Ready,
        });
        assert!(snapshot.has_primary_account());
        assert!(snapshot.telegram_ready());
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
        assert!(commands.contains(&AdapterCommand::WhatsAppBeginLink));
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
        assert!(!row.linked);
        assert_ne!(row.status, AdapterStatus::Ready);
        snapshot.cancel_whatsapp_link(&phone);
        assert_eq!(snapshot.whatsapp_screen, WhatsAppScreen::Hidden);
        assert!(phone.phone().is_none());
        assert!(snapshot.whatsapp_qr.is_none());
    }

    #[test]
    fn send_compose_queues_text_on_the_worker_without_a_local_stub() {
        let mut snapshot = Snapshot::new();
        snapshot.telegram_authorized = true;
        snapshot.selected_protocol = ProtocolId::Telegram;
        snapshot.selected_conversation = Some("telegram:42".into());
        snapshot.compose = " hello ".into();
        snapshot.send_compose();
        assert!(snapshot.compose.is_empty());
        assert!(snapshot.selected_messages().is_empty());
        let commands = snapshot.take_commands();
        assert!(commands.iter().any(|command| matches!(
            command,
            AdapterCommand::SendText {
                protocol: ProtocolId::Telegram,
                conversation_id,
                body,
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
        snapshot.send_compose();
        assert_eq!(snapshot.compose, "hello");
        assert!(snapshot.take_commands().is_empty());
        assert!(snapshot.error.is_some());
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
            .linked = true;
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: Conversation {
                protocol: ProtocolId::Telegram,
                id: "telegram:2".into(),
                title: "Older".into(),
                participant: "Older".into(),
                preview: "a".into(),
                unread: 0,
                order: 10,
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
            .linked = true;
        snapshot.apply(AdapterEvent::ConversationUpsert {
            conversation: Conversation {
                protocol: ProtocolId::Telegram,
                id: "telegram:3".into(),
                title: "Gone".into(),
                participant: "Gone".into(),
                preview: String::new(),
                unread: 2,
                order: 5,
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
}
