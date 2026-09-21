//! UI-side snapshot. Mutated only on the UI thread from polled events and clicks.

use std::collections::{HashMap, HashSet};

use thinwire_protocol::{
    AdapterCommand, AdapterEvent, AdapterStatus, ChatMessage, Conversation, ProtocolCapabilities,
    ProtocolId, TelegramApiSource, TelegramAuthPhase, TelegramAuthStep, TelegramSecretVault,
    catalog, telegram_api_available,
};

use super::secrets::{SecretKey, SecretStore};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InboxFilter {
    All,
    Telegram,
    Slack,
    Experimental,
}

impl InboxFilter {
    pub(crate) const ALL: [Self; 4] = [Self::All, Self::Telegram, Self::Slack, Self::Experimental];

    #[must_use]
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Telegram => "Telegram",
            Self::Slack => "Slack",
            Self::Experimental => "Experimental",
        }
    }

    #[must_use]
    pub(crate) const fn matches(self, protocol: ProtocolId) -> bool {
        match self {
            Self::All => true,
            Self::Telegram => matches!(protocol, ProtocolId::Telegram),
            Self::Slack => matches!(protocol, ProtocolId::Slack),
            Self::Experimental => matches!(protocol, ProtocolId::WhatsApp | ProtocolId::Discord),
        }
    }

    /// Inbox filters hide supported accounts; experimental chips stay in the switcher.
    #[must_use]
    pub(crate) const fn shows_in_switcher(self, protocol: ProtocolId) -> bool {
        self.matches(protocol) || matches!(protocol, ProtocolId::WhatsApp | ProtocolId::Discord)
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
    pub telegram_authorized: bool,
    api_source: TelegramApiSource,
    pending: Vec<AdapterCommand>,
    keychain_flush: bool,
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
            status_text: "Adapters are stubs. No live network session.".into(),
            compose: String::new(),
            auth_busy: false,
            telegram_authorized: false,
            api_source: TelegramApiSource::from_build(),
            pending: Vec::new(),
            keychain_flush: false,
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
                }
                self.status_text = detail;
            }
            AdapterEvent::ConversationUpsert { conversation } => {
                let protocol = conversation.protocol;
                let list = self.conversations.entry(protocol).or_default();
                if let Some(existing) = list.iter_mut().find(|row| row.id == conversation.id) {
                    *existing = conversation;
                } else {
                    list.push(conversation);
                }
                self.ensure_conversation_selection();
            }
            AdapterEvent::MessageReceived { message } => {
                let key = (message.protocol, message.conversation_id.clone());
                self.messages.entry(key).or_default().push(message);
            }
            AdapterEvent::TelegramAuth { phase } => self.apply_telegram_phase(phase),
        }
    }

    pub(crate) fn take_commands(&mut self) -> Vec<AdapterCommand> {
        std::mem::take(&mut self.pending)
    }

    #[must_use]
    pub(crate) fn take_keychain_flush(&mut self) -> bool {
        std::mem::take(&mut self.keychain_flush)
    }

    pub(crate) fn has_primary_account(&self) -> bool {
        self.accounts
            .iter()
            .any(|row| row.linked && matches!(row.caps.id, ProtocolId::Telegram))
    }

    pub(crate) fn select_protocol(&mut self, protocol: ProtocolId) {
        self.selected_protocol = protocol;
        self.selected_conversation = None;
        self.ensure_conversation_selection();
    }

    pub(crate) fn select_conversation(&mut self, id: String) {
        self.selected_conversation = Some(id);
    }

    pub(crate) fn set_filter(&mut self, filter: InboxFilter) {
        self.filter = filter;
        if !filter.matches(self.selected_protocol)
            && let Some(first) = self
                .accounts
                .iter()
                .find(|row| filter.matches(row.caps.id))
                .map(|row| row.caps.id)
        {
            self.select_protocol(first);
        }
    }

    pub(crate) fn visible_conversations(&self) -> Vec<&Conversation> {
        if !self.protocol_linked(self.selected_protocol) {
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

    pub(crate) fn selected_account(&self) -> Option<&AccountRow> {
        self.accounts
            .iter()
            .find(|row| row.caps.id == self.selected_protocol)
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
            .filter(|id| self.filter.matches(*id))
            .collect();
        for protocol in protocols {
            self.pending.push(AdapterCommand::Connect { protocol });
        }
        self.status_text = "Refresh queued on the tokio worker. The UI thread stays free.".into();
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

    pub(crate) fn send_compose_stub(&mut self) {
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
                "Type a message for the selected thread. This stub does not open a live session.",
            );
            return;
        }
        self.compose.clear();
        self.error = None;
        let id = format!("{conversation_id}:compose-stub");
        self.messages
            .entry((self.selected_protocol, conversation_id.clone()))
            .or_default()
            .push(ChatMessage {
                protocol: self.selected_protocol,
                conversation_id,
                id,
                sender: "you".into(),
                body,
                outbound: true,
            });
        self.status_text =
            "Compose stub queued on the UI snapshot only. No protocol I/O ran.".into();
    }

    pub(crate) fn open_telegram(&mut self, store: &SecretStore) {
        self.clear_secrets();
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
        self.error = None;
        match phase {
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
        self.select_protocol(ProtocolId::Telegram);
        self.auth = AuthScreen::Idle;
        self.auth_busy = false;
        self.telegram_authorized = true;
        self.error = None;
        self.status_text = "Telegram is ready. TDLib session is live.".into();
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
        }
    }
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
        assert!(ui.contains("not login peers"));
        assert!(!ui.contains("my.telegram.org"));
        assert!(src.contains(super::super::auth::TDLIB_UNAVAILABLE_BANNER));
        assert!(ui.contains("stub_banner"));
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
        let seven = include_str!("../../../../decisions/0007-publisher-telegram-api-credentials.md");
        assert!(seven.contains("# Publisher-owned Telegram api_id / api_hash"));
        assert!(seven.contains("Primary login UI: phone/code"));
        assert!(seven.contains("not** the primary login path"));
        assert!(seven.contains("do **not** send every user to my.telegram.org"));
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
    fn experimental_filter_hides_supported_protocols() {
        assert!(InboxFilter::Experimental.matches(ProtocolId::WhatsApp));
        assert!(!InboxFilter::Experimental.matches(ProtocolId::Telegram));
        assert!(InboxFilter::Telegram.matches(ProtocolId::Telegram));
        assert!(InboxFilter::All.matches(ProtocolId::Discord));
    }

    #[test]
    fn experimental_chips_stay_in_switcher_under_every_filter() {
        for filter in InboxFilter::ALL {
            assert!(filter.shows_in_switcher(ProtocolId::WhatsApp));
            assert!(filter.shows_in_switcher(ProtocolId::Discord));
        }
        assert!(!InboxFilter::Telegram.shows_in_switcher(ProtocolId::Slack));
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
    fn compose_stub_appends_outbound_without_protocol_command() {
        let mut snapshot = Snapshot::new();
        snapshot.selected_conversation = Some("telegram:saved".into());
        snapshot.compose = "hello".into();
        snapshot.send_compose_stub();
        assert!(snapshot.compose.is_empty());
        assert!(snapshot.take_commands().is_empty());
        let messages = snapshot.selected_messages();
        assert_eq!(messages.last().map(|m| m.body.as_str()), Some("hello"));
        assert_eq!(messages.last().map(|m| m.outbound), Some(true));
    }
}
