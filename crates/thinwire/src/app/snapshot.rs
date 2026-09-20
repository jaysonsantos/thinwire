//! UI-side snapshot. Mutated only on the UI thread from polled events and clicks.

use std::collections::{HashMap, HashSet};

use thinwire_protocol::{
    AdapterCommand, AdapterEvent, AdapterStatus, CRITIC_BULLET_1, ChatMessage, Conversation,
    DiscordAuthMode, ProtocolCapabilities, ProtocolId, catalog, critic_bullets_for,
    requires_experimental_gate,
};

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

/// Non-modal auth steps. Credential field values never leave this snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthScreen {
    Idle,
    ChooseProtocol,
    ExperimentalGate { protocol: ProtocolId },
    TelegramApi,
    TelegramPhone,
    TelegramCode,
    Telegram2fa,
    WhatsAppQr,
    DiscordChoose,
    DiscordBot,
    DiscordOAuth,
    SlackOAuth,
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
    pub risk_understood: bool,
    pub telegram_api_id: String,
    pub telegram_api_hash: String,
    pub telegram_phone: String,
    pub telegram_code: String,
    pub telegram_2fa: String,
    pub error: Option<UserError>,
    pub status_text: String,
    pub compose: String,
    pending: Vec<AdapterCommand>,
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
            risk_understood: false,
            telegram_api_id: String::new(),
            telegram_api_hash: String::new(),
            telegram_phone: String::new(),
            telegram_code: String::new(),
            telegram_2fa: String::new(),
            error: None,
            status_text: "Adapters are stubs. No live network session.".into(),
            compose: String::new(),
            pending: Vec::new(),
        }
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
        }
    }

    pub(crate) fn take_commands(&mut self) -> Vec<AdapterCommand> {
        std::mem::take(&mut self.pending)
    }

    pub(crate) fn has_primary_account(&self) -> bool {
        self.accounts.iter().any(|row| {
            row.linked && matches!(row.caps.id, ProtocolId::Telegram | ProtocolId::Slack)
        })
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

    pub(crate) fn open_add_account(&mut self) {
        self.error = None;
        self.risk_understood = false;
        self.auth = AuthScreen::ChooseProtocol;
        self.status_text =
            "Add account — pick Telegram or Slack, or an experimental module.".into();
    }

    pub(crate) fn start_supported(&mut self, protocol: ProtocolId) {
        match protocol {
            ProtocolId::Telegram => self.open_telegram(),
            ProtocolId::Slack => self.open_slack(),
            other => self.choose_protocol(other),
        }
    }

    pub(crate) fn choose_protocol(&mut self, protocol: ProtocolId) {
        self.error = None;
        self.risk_understood = false;
        if requires_experimental_gate(protocol) {
            self.auth = AuthScreen::ExperimentalGate { protocol };
            self.status_text = format!(
                "Risk gate for {name}. Read the notice before any QR or token step.",
                name = protocol.display_name()
            );
            return;
        }
        match protocol {
            ProtocolId::Telegram => self.open_telegram(),
            ProtocolId::Slack => self.open_slack(),
            ProtocolId::WhatsApp | ProtocolId::Discord => {
                self.set_error(
                    "Experimental gate was skipped.",
                    "WhatsApp and Discord must show Critic risk facts first.",
                    "Use Add account and accept the risk checkbox.",
                );
            }
        }
    }

    pub(crate) fn continue_experimental(&mut self) -> Result<(), UserError> {
        let AuthScreen::ExperimentalGate { protocol } = self.auth else {
            return Err(self.gate_error(
                "No experimental gate is open.",
                "Continue only applies on the risk step.",
                "Choose an experimental protocol from Add account.",
            ));
        };
        if !self.risk_understood {
            let err = self.gate_error(
                "Continue is blocked.",
                "The risk checkbox is still unchecked.",
                "Read the Critic notice and tick “I understand the risk”.",
            );
            self.error = Some(err.clone());
            return Err(err);
        }
        match protocol {
            ProtocolId::WhatsApp => {
                self.auth = AuthScreen::WhatsAppQr;
                self.status_text =
                    "WhatsApp linked-device placeholder. No QR session is live.".into();
            }
            ProtocolId::Discord => {
                self.auth = AuthScreen::DiscordChoose;
                self.status_text =
                    "Discord bot/OAuth inbox only. User-account login is refused.".into();
            }
            ProtocolId::Telegram | ProtocolId::Slack => {
                return Err(self.gate_error(
                    "This protocol has no experimental gate.",
                    "Telegram and Slack are supported paths.",
                    "Add them from the first-run buttons.",
                ));
            }
        }
        self.error = None;
        Ok(())
    }

    pub(crate) fn cancel_auth(&mut self) {
        self.clear_secrets();
        self.auth = AuthScreen::Idle;
        self.risk_understood = false;
        self.error = None;
        self.status_text = "Account linking cancelled.".into();
    }

    pub(crate) fn advance_telegram(&mut self) {
        match self.auth {
            AuthScreen::TelegramApi => {
                self.auth = AuthScreen::TelegramPhone;
                self.status_text =
                    "Telegram stub: enter a phone number locally. Nothing is stored.".into();
            }
            AuthScreen::TelegramPhone => {
                self.auth = AuthScreen::TelegramCode;
                self.status_text = "Telegram stub: code step. TDLib is not connected.".into();
            }
            AuthScreen::TelegramCode => {
                self.auth = AuthScreen::Telegram2fa;
                self.status_text = "Telegram stub: optional 2FA. Leave blank to skip.".into();
            }
            AuthScreen::Telegram2fa => self.finish_stub(
                ProtocolId::Telegram,
                "Telegram stub linked. TDLib client not started; fields were discarded.",
            ),
            _ => {}
        }
    }

    pub(crate) fn finish_whatsapp_placeholder(&mut self) {
        self.finish_stub(
            ProtocolId::WhatsApp,
            "WhatsApp stub noted. Unofficial linked-device path; no live QR.",
        );
    }

    pub(crate) fn open_discord_bot(&mut self) {
        self.auth = AuthScreen::DiscordBot;
        self.status_text = "Discord bot stub. No token is stored or requested.".into();
    }

    pub(crate) fn open_discord_oauth(&mut self) {
        self.auth = AuthScreen::DiscordOAuth;
        self.status_text = "Discord OAuth stub. User-account login is not offered.".into();
    }

    pub(crate) fn finish_discord(&mut self, mode: DiscordAuthMode) {
        if mode == DiscordAuthMode::UserAccount {
            self.set_error(
                "Discord user-account login was refused.",
                CRITIC_BULLET_1,
                "Use a bot token or OAuth app owned by you. Never paste a user token.",
            );
            return;
        }
        self.pending.push(AdapterCommand::ConnectDiscord { mode });
        self.finish_stub(
            ProtocolId::Discord,
            "Discord bot/OAuth inbox stub noted. No user-account session.",
        );
    }

    pub(crate) fn finish_slack(&mut self) {
        self.finish_stub(
            ProtocolId::Slack,
            "Slack OAuth stub noted. Workspace app path; not a personal desktop clone.",
        );
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

    pub(crate) fn critic_lines(&self) -> &'static [&'static str] {
        match self.auth {
            AuthScreen::ExperimentalGate { protocol } => critic_bullets_for(protocol),
            _ => &[],
        }
    }

    fn open_telegram(&mut self) {
        self.clear_secrets();
        self.auth = AuthScreen::TelegramApi;
        self.status_text =
            "Telegram (TDLib): create an app at my.telegram.org, then enter api_id and api_hash here. Values stay on this machine and are discarded when you cancel or finish.".into();
    }

    fn open_slack(&mut self) {
        self.auth = AuthScreen::SlackOAuth;
        self.status_text =
            "Slack workspace OAuth placeholder. Browser sign-in is not started in this revision."
                .into();
    }

    fn finish_stub(&mut self, protocol: ProtocolId, status: &str) {
        self.clear_secrets();
        if let Some(row) = self.accounts.iter_mut().find(|row| row.caps.id == protocol) {
            row.linked = true;
        }
        self.pending.push(AdapterCommand::Connect { protocol });
        self.select_protocol(protocol);
        self.auth = AuthScreen::Idle;
        self.risk_understood = false;
        self.error = None;
        self.status_text = status.into();
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

    fn gate_error(&self, happened: &str, why: &str, next: &str) -> UserError {
        UserError {
            happened: happened.into(),
            why: why.into(),
            next: next.into(),
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn experimental_auth_requires_risk_checkbox() {
        let mut snapshot = Snapshot::new();
        snapshot.choose_protocol(ProtocolId::WhatsApp);
        assert!(matches!(
            snapshot.auth,
            AuthScreen::ExperimentalGate {
                protocol: ProtocolId::WhatsApp
            }
        ));
        assert!(snapshot.continue_experimental().is_err());
        assert!(matches!(snapshot.auth, AuthScreen::ExperimentalGate { .. }));
        snapshot.risk_understood = true;
        snapshot.continue_experimental().expect("gate opens");
        assert_eq!(snapshot.auth, AuthScreen::WhatsAppQr);
    }

    #[test]
    fn discord_auth_source_has_no_user_token_field() {
        let src = include_str!("auth.rs");
        assert!(!src.contains("UserAccount"));
        assert!(!src.contains("user_token"));
        assert!(src.contains("DiscordAuthMode::Bot"));
        assert!(src.contains("DiscordAuthMode::OAuth"));
        assert!(src.contains("No user-token field exists"));
    }

    #[test]
    fn discord_ui_never_offers_user_account_mode() {
        let mut snapshot = Snapshot::new();
        snapshot.risk_understood = true;
        snapshot.auth = AuthScreen::ExperimentalGate {
            protocol: ProtocolId::Discord,
        };
        snapshot.continue_experimental().expect("discord gate");
        assert_eq!(snapshot.auth, AuthScreen::DiscordChoose);
        snapshot.finish_discord(DiscordAuthMode::UserAccount);
        assert!(snapshot.error.is_some());
        assert!(
            !snapshot
                .accounts
                .iter()
                .any(|row| row.caps.id == ProtocolId::Discord && row.linked)
        );
    }

    #[test]
    fn telegram_and_slack_skip_experimental_gate() {
        let mut snapshot = Snapshot::new();
        snapshot.start_supported(ProtocolId::Telegram);
        assert_eq!(snapshot.auth, AuthScreen::TelegramApi);
        snapshot.start_supported(ProtocolId::Slack);
        assert_eq!(snapshot.auth, AuthScreen::SlackOAuth);
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
    fn whatsapp_gate_uses_all_three_critic_bullets() {
        let mut snapshot = Snapshot::new();
        snapshot.choose_protocol(ProtocolId::WhatsApp);
        let lines = snapshot.critic_lines();
        assert_eq!(lines, thinwire_protocol::CRITIC_RISK_BULLETS.as_slice());
    }

    #[test]
    fn discord_gate_uses_all_three_critic_bullets() {
        let mut snapshot = Snapshot::new();
        snapshot.choose_protocol(ProtocolId::Discord);
        assert_eq!(
            snapshot.critic_lines(),
            thinwire_protocol::CRITIC_RISK_BULLETS.as_slice()
        );
    }

    #[test]
    fn discord_user_account_error_quotes_critic_bullet_one() {
        let mut snapshot = Snapshot::new();
        snapshot.finish_discord(DiscordAuthMode::UserAccount);
        let error = snapshot.error.expect("refused");
        assert_eq!(error.happened, "Discord user-account login was refused.");
        assert_eq!(error.why, CRITIC_BULLET_1);
        assert!(!error.next.is_empty());
    }

    #[test]
    fn first_run_telegram_lands_in_first_inbox() {
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
        snapshot.auth = AuthScreen::Telegram2fa;
        snapshot.advance_telegram();
        assert!(snapshot.has_primary_account());
        assert_eq!(snapshot.selected_protocol, ProtocolId::Telegram);
        assert_eq!(
            snapshot.selected_conversation.as_deref(),
            Some("telegram:saved")
        );
        assert_eq!(snapshot.visible_conversations().len(), 1);
        assert_eq!(snapshot.unread_for(ProtocolId::Telegram), 2);
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
