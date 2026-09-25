//! In-process Signal device used by tests and by builds without `signal-local`.
//!
//! The provisioning URL and message text stay out of logs. Callers emit the
//! URL only as a redacted event.

#[cfg(test)]
use std::collections::HashMap;

#[cfg(test)]
use crate::adapter::Delivery;
use crate::adapter::{ChatMessage, Conversation};

/// Sync device. The live presage client does not implement this trait.
pub(crate) trait SignalDevice: Send {
    /// `Ok(Some(url))` is a provisioning URL. `Ok(None)` means already linked.
    fn link(&mut self) -> Result<Option<String>, &'static str>;
    fn chats(&mut self) -> Result<Vec<Conversation>, &'static str>;
    fn history(&mut self, conversation_id: &str) -> Result<Vec<ChatMessage>, &'static str>;
    fn send(&mut self, conversation_id: &str, body: &str) -> Result<ChatMessage, &'static str>;
    fn resend(
        &mut self,
        conversation_id: &str,
        message_id: &str,
    ) -> Result<ChatMessage, &'static str>;
}

#[cfg(not(feature = "signal-local"))]
pub(crate) struct FeatureOff;

#[cfg(not(feature = "signal-local"))]
impl SignalDevice for FeatureOff {
    fn link(&mut self) -> Result<Option<String>, &'static str> {
        Err(super::FEATURE_OFF)
    }

    fn chats(&mut self) -> Result<Vec<Conversation>, &'static str> {
        Err(super::FEATURE_OFF)
    }

    fn history(&mut self, _conversation_id: &str) -> Result<Vec<ChatMessage>, &'static str> {
        Err(super::FEATURE_OFF)
    }

    fn send(&mut self, _conversation_id: &str, _body: &str) -> Result<ChatMessage, &'static str> {
        Err(super::FEATURE_OFF)
    }

    fn resend(
        &mut self,
        _conversation_id: &str,
        _message_id: &str,
    ) -> Result<ChatMessage, &'static str> {
        Err(super::FEATURE_OFF)
    }
}

/// Test double. Holds chats in memory and never opens a network session.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct FakeDevice {
    url: Option<String>,
    chats: Vec<Conversation>,
    history: HashMap<String, Vec<ChatMessage>>,
    linked: bool,
}

#[cfg(test)]
impl FakeDevice {
    pub(crate) fn with_inbox(
        url: impl Into<String>,
        chats: Vec<Conversation>,
        history: HashMap<String, Vec<ChatMessage>>,
    ) -> Self {
        Self {
            url: Some(url.into()),
            chats,
            history,
            linked: false,
        }
    }
}

#[cfg(test)]
impl SignalDevice for FakeDevice {
    fn link(&mut self) -> Result<Option<String>, &'static str> {
        self.linked = true;
        Ok(self.url.clone())
    }

    fn chats(&mut self) -> Result<Vec<Conversation>, &'static str> {
        if !self.linked {
            return Err("Signal is not linked");
        }
        Ok(self.chats.clone())
    }

    fn history(&mut self, conversation_id: &str) -> Result<Vec<ChatMessage>, &'static str> {
        if !self.linked {
            return Err("Signal is not linked");
        }
        Ok(self
            .history
            .get(conversation_id)
            .cloned()
            .unwrap_or_default())
    }

    fn send(&mut self, conversation_id: &str, body: &str) -> Result<ChatMessage, &'static str> {
        if !self.linked {
            return Err("Signal is not linked");
        }
        if !self.chats.iter().any(|chat| chat.id == conversation_id) {
            return Err("Signal chat was not found");
        }
        let message = ChatMessage {
            protocol: crate::ProtocolId::Signal,
            conversation_id: conversation_id.to_string(),
            id: format!("signal:{conversation_id}:sent"),
            sender: "me".into(),
            body: body.to_string(),
            outbound: true,
            delivery: Delivery::Sent,
            sent_at: 0,
        };
        self.history
            .entry(conversation_id.to_string())
            .or_default()
            .push(message.clone());
        Ok(message)
    }

    fn resend(
        &mut self,
        conversation_id: &str,
        message_id: &str,
    ) -> Result<ChatMessage, &'static str> {
        let body = self
            .history
            .get(conversation_id)
            .and_then(|rows| rows.iter().find(|row| row.id == message_id))
            .map(|row| row.body.clone());
        let Some(body) = body else {
            return Err("Signal message was not found");
        };
        self.send(conversation_id, &body)
    }
}
