// SPDX-License-Identifier: AGPL-3.0-only
#![cfg_attr(not(feature = "signal-local"), allow(dead_code))]
//! Signal group v2 threads. The conversation id is a hash. The master key stays in the adapter.

use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GroupChat {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) is_group: bool,
}

#[must_use]
pub(crate) fn group_chat(master_key: &[u8], title: &str) -> GroupChat {
    let title = if title.is_empty() {
        "Signal group".to_string()
    } else {
        title.to_string()
    };
    GroupChat {
        id: group_id(master_key),
        title,
        is_group: true,
    }
}

/// Contact profile name when the store has one. Otherwise the sender uuid.
#[must_use]
pub(crate) fn sender_name(uuid: &str, names: &HashMap<String, String>) -> String {
    names
        .get(uuid)
        .filter(|name| !name.is_empty())
        .cloned()
        .unwrap_or_else(|| uuid.to_string())
}

const GROUP_PREFIX: &str = "signal:group:";

/// Master keys for group ids. `Debug` prints the count only.
#[derive(Default)]
pub(crate) struct GroupKeys {
    by_id: HashMap<String, [u8; 32]>,
}

impl GroupKeys {
    /// Store `master_key` and return the public conversation id.
    pub(crate) fn remember(&mut self, master_key: &[u8]) -> String {
        let id = group_id(master_key);
        if let Ok(key) = <[u8; 32]>::try_from(master_key) {
            self.by_id.insert(id.clone(), key);
        }
        id
    }

    #[must_use]
    pub(crate) fn key(&self, id: &str) -> Option<[u8; 32]> {
        self.by_id.get(id).copied()
    }
}

impl std::fmt::Debug for GroupKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GroupKeys")
            .field("groups", &self.by_id.len())
            .finish()
    }
}

/// A group conversation id. A contact id does not use this prefix.
#[must_use]
pub(crate) fn is_group_id(conversation_id: &str) -> bool {
    conversation_id.starts_with(GROUP_PREFIX)
}

/// Public id for a group. The suffix is 16 bytes of SHA-256, not the master key.
#[must_use]
pub(crate) fn group_id(master_key: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(master_key);
    format!("{GROUP_PREFIX}{}", hex(&digest[..16]))
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_group_thread_keeps_its_title_and_id() {
        let key = [0xab, 0x0c, 0x11, 0xff];
        let chat = group_chat(&key, "Family");
        assert!(chat.is_group);
        assert_eq!(chat.title, "Family");
        assert!(chat.id.starts_with("signal:group:"));
        assert_eq!(group_chat(&key, "").title, "Signal group");
    }

    #[test]
    fn a_group_id_hides_the_master_key() {
        use thinwire_protocol::{
            AccountState, AdapterCommand, AdapterError, AdapterEvent, AdapterStatus, ChatMessage,
            Conversation, Delivery, DiscordAuthMode, ProtocolId, RedactedPairingSecret,
            TelegramAuthError, TelegramAuthPhase, TelegramAuthStep, TelegramCodeVia,
        };

        let key = [0xA5; 32];
        let hex_key = "a5".repeat(32);
        let b64_key = standard_base64(&key);
        let id = group_id(&key);
        let suffix = id.strip_prefix(GROUP_PREFIX).expect("prefix");
        assert_eq!(suffix.len(), 32);
        assert!(suffix.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(is_group_id(&id));
        assert!(!is_group_id("11111111-1111-1111-1111-111111111111"));
        assert!(!id.contains(&hex_key));
        assert!(!id.contains(&b64_key));
        let mut keys = GroupKeys::default();
        assert_eq!(keys.remember(&key), id);
        assert_eq!(keys.key(&id), Some(key));

        let protocol = ProtocolId::Signal;
        let chat = Conversation {
            protocol,
            id: id.clone(),
            title: "Book club".into(),
            participant: id.clone(),
            preview: "hi".into(),
            unread: 1,
            order: 1,
            last_at: 1,
            is_group: true,
            writable: true,
            muted: false,
            placeholder: false,
        };
        let message = ChatMessage {
            protocol,
            conversation_id: id.clone(),
            id: "1000".into(),
            sender: "Ada".into(),
            body: "hi".into(),
            outbound: false,
            delivery: Delivery::Sent,
            sent_at: 1,
            arrival: thinwire_protocol::Arrival::History,
        };
        let commands = [
            AdapterCommand::Connect { protocol },
            AdapterCommand::Disconnect { protocol },
            AdapterCommand::ConnectDiscord {
                mode: DiscordAuthMode::Bot,
            },
            AdapterCommand::TelegramAuth {
                step: TelegramAuthStep::Complete,
                epoch: 1,
            },
            AdapterCommand::LoadChats { protocol },
            AdapterCommand::OpenChat {
                protocol,
                conversation_id: id.clone(),
            },
            AdapterCommand::Shutdown { protocol },
            AdapterCommand::LoadOlderMessages {
                protocol,
                conversation_id: id.clone(),
                before_message_id: "1000".into(),
            },
            AdapterCommand::ResendMessage {
                protocol,
                conversation_id: id.clone(),
                message_id: "1000".into(),
                request: 1,
            },
            AdapterCommand::ViewChat {
                protocol,
                conversation_id: Some(id.clone()),
            },
            AdapterCommand::SendText {
                protocol,
                conversation_id: id.clone(),
                body: "hi".into(),
                request: 1,
            },
            AdapterCommand::WhatsAppAcknowledgeRisk,
            AdapterCommand::WhatsAppBeginLink { generation: 1 },
            AdapterCommand::WhatsAppCancelLink,
            AdapterCommand::SignalAcknowledgeNotice,
            AdapterCommand::SignalBeginLink { generation: 1 },
            AdapterCommand::SignalCancelLink,
        ];
        let events = [
            AdapterEvent::Status {
                protocol,
                status: AdapterStatus::Ready,
                detail: id.clone(),
            },
            AdapterEvent::ConversationUpsert {
                conversation: chat.clone(),
            },
            AdapterEvent::MessageReceived {
                message: message.clone(),
            },
            AdapterEvent::TelegramAuth {
                phase: TelegramAuthPhase::Ready,
            },
            AdapterEvent::TelegramAuthRejected {
                error: TelegramAuthError::Other { code: 1 },
            },
            AdapterEvent::TelegramDataReset {
                moved_to: "tdlib.stale".into(),
            },
            AdapterEvent::TelegramSessionEnded,
            AdapterEvent::Login {
                epoch: 1,
                event: Box::new(AdapterEvent::Status {
                    protocol,
                    status: AdapterStatus::Ready,
                    detail: id.clone(),
                }),
            },
            AdapterEvent::TelegramCodeSent {
                via: TelegramCodeVia::Sms,
            },
            AdapterEvent::FlushSecrets,
            AdapterEvent::ConversationRemoved {
                protocol,
                id: id.clone(),
            },
            AdapterEvent::MessageReplaced {
                protocol,
                conversation_id: id.clone(),
                old_id: "1".into(),
                message: message.clone(),
            },
            AdapterEvent::MessageBody {
                protocol,
                conversation_id: id.clone(),
                message_id: "1000".into(),
                body: "hi".into(),
            },
            AdapterEvent::MessagesRemoved {
                protocol,
                conversation_id: id.clone(),
                message_ids: vec!["1000".into()],
            },
            AdapterEvent::SendAccepted {
                protocol,
                conversation_id: id.clone(),
                request: 1,
            },
            AdapterEvent::SendRejected {
                protocol,
                conversation_id: id.clone(),
                request: 1,
            },
            AdapterEvent::Stopped { protocol },
            AdapterEvent::Account {
                protocol,
                state: AccountState::Linked,
            },
            AdapterEvent::CommandFailed {
                protocol,
                conversation_id: Some(id.clone()),
                detail: id.clone(),
            },
            AdapterEvent::Notice {
                protocol,
                text: id.clone(),
            },
            AdapterEvent::MessageDelivery {
                protocol,
                conversation_id: id.clone(),
                message_id: "1000".into(),
                delivery: Delivery::Sent,
            },
            AdapterEvent::ChatListLoaded { protocol },
            AdapterEvent::OlderHistoryLoaded {
                protocol,
                conversation_id: id.clone(),
                before_message_id: "1000".into(),
                more: false,
                note: Some(id.clone()),
            },
            AdapterEvent::HistoryLoaded {
                protocol,
                conversation_id: id.clone(),
            },
            AdapterEvent::WhatsAppQr {
                code: RedactedPairingSecret::new("qr"),
                generation: 1,
            },
            AdapterEvent::WhatsAppPairCode {
                code: RedactedPairingSecret::new("pair"),
                generation: 1,
            },
            AdapterEvent::SignalQr {
                code: RedactedPairingSecret::new("signal"),
                generation: 1,
            },
        ];
        let errors = [
            AdapterError::Refused {
                protocol,
                reason: "refused",
            },
            AdapterError::Unavailable {
                protocol,
                reason: "unavailable",
            },
        ];
        let mut rendered = format!("{chat:?} {message:?} {keys:?}");
        for command in &commands {
            rendered.push_str(&format!(" {command:?}"));
        }
        for event in &events {
            rendered.push_str(&format!(" {event:?}"));
        }
        for error in &errors {
            rendered.push_str(&format!(" {error:?} {error}"));
        }
        assert!(!rendered.contains(&hex_key), "{rendered}");
        assert!(!rendered.contains(&hex_key.to_uppercase()), "{rendered}");
        assert!(!rendered.contains(&b64_key), "{rendered}");
        assert!(!rendered.contains("165, 165"), "{rendered}");
    }

    fn standard_base64(data: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        let mut index = 0;
        while index + 3 <= data.len() {
            let chunk = ((data[index] as u32) << 16)
                | ((data[index + 1] as u32) << 8)
                | (data[index + 2] as u32);
            out.push(ALPHABET[((chunk >> 18) & 63) as usize] as char);
            out.push(ALPHABET[((chunk >> 12) & 63) as usize] as char);
            out.push(ALPHABET[((chunk >> 6) & 63) as usize] as char);
            out.push(ALPHABET[(chunk & 63) as usize] as char);
            index += 3;
        }
        if index < data.len() {
            let rest = data.len() - index;
            let mut chunk = (data[index] as u32) << 16;
            if rest == 2 {
                chunk |= (data[index + 1] as u32) << 8;
            }
            out.push(ALPHABET[((chunk >> 18) & 63) as usize] as char);
            out.push(ALPHABET[((chunk >> 12) & 63) as usize] as char);
            if rest == 2 {
                out.push(ALPHABET[((chunk >> 6) & 63) as usize] as char);
                out.push('=');
            } else {
                out.push('=');
                out.push('=');
            }
        }
        out
    }

    #[test]
    fn a_group_message_uses_the_contact_name() {
        let mut names = HashMap::new();
        names.insert(
            "11111111-1111-1111-1111-111111111111".to_string(),
            "Ada".to_string(),
        );
        assert_eq!(
            sender_name("11111111-1111-1111-1111-111111111111", &names),
            "Ada"
        );
        assert_eq!(
            sender_name("22222222-2222-2222-2222-222222222222", &names),
            "22222222-2222-2222-2222-222222222222"
        );
    }
}
