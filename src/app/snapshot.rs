//! UI-side snapshot. Mutated only on the UI thread from polled events.

use std::collections::HashMap;

use crate::protocols::{
    catalog, AdapterEvent, AdapterStatus, ChatMessage, Conversation, ProtocolCapabilities,
    ProtocolId,
};

#[derive(Debug, Clone)]
pub(crate) struct AccountRow {
    pub caps: ProtocolCapabilities,
    pub status: AdapterStatus,
    pub detail: String,
}

#[derive(Debug)]
pub(crate) struct Snapshot {
    pub accounts: Vec<AccountRow>,
    conversations: HashMap<ProtocolId, Vec<Conversation>>,
    messages: HashMap<(ProtocolId, String), Vec<ChatMessage>>,
    pub selected_protocol: ProtocolId,
    pub selected_conversation: Option<String>,
}

impl Snapshot {
    pub(crate) fn new() -> Self {
        let accounts = catalog()
            .into_iter()
            .map(|caps| AccountRow {
                caps,
                status: AdapterStatus::Stubbed,
                detail: caps.detail.to_string(),
            })
            .collect();
        Self {
            accounts,
            conversations: HashMap::new(),
            messages: HashMap::new(),
            selected_protocol: ProtocolId::Telegram,
            selected_conversation: None,
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
                    row.detail = detail;
                }
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

    pub(crate) fn select_protocol(&mut self, protocol: ProtocolId) {
        self.selected_protocol = protocol;
        self.selected_conversation = None;
        self.ensure_conversation_selection();
    }

    pub(crate) fn select_conversation(&mut self, id: String) {
        self.selected_conversation = Some(id);
    }

    pub(crate) fn conversations(&self) -> &[Conversation] {
        self.conversations
            .get(&self.selected_protocol)
            .map_or(&[], Vec::as_slice)
    }

    pub(crate) fn selected_account(&self) -> Option<&AccountRow> {
        self.accounts
            .iter()
            .find(|row| row.caps.id == self.selected_protocol)
    }

    pub(crate) fn selected_conversation_row(&self) -> Option<&Conversation> {
        let id = self.selected_conversation.as_ref()?;
        self.conversations().iter().find(|row| row.id == *id)
    }

    pub(crate) fn selected_messages(&self) -> &[ChatMessage] {
        let Some(id) = self.selected_conversation.as_ref() else {
            return &[];
        };
        self.messages
            .get(&(self.selected_protocol, id.clone()))
            .map_or(&[], Vec::as_slice)
    }

    fn ensure_conversation_selection(&mut self) {
        if self.selected_conversation.is_some() {
            return;
        }
        if let Some(first) = self.conversations().first() {
            self.selected_conversation = Some(first.id.clone());
        }
    }
}
