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

/// Public id for a group. This is a hash. It is not the master key.
#[must_use]
pub(crate) fn group_id(master_key: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{GROUP_PREFIX}{}", hex(&Sha256::digest(master_key)))
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
        let key = [0xA5; 32];
        let secret = "a5".repeat(32);
        let id = group_id(&key);
        assert!(is_group_id(&id));
        assert!(!is_group_id("11111111-1111-1111-1111-111111111111"));
        assert!(!id.contains(&secret));
        let mut keys = GroupKeys::default();
        assert_eq!(keys.remember(&key), id);
        assert_eq!(keys.key(&id), Some(key));
        let command = thinwire_protocol::AdapterCommand::SendText {
            protocol: thinwire_protocol::ProtocolId::Signal,
            conversation_id: id.clone(),
            body: "hi".into(),
            request: 1,
        };
        let event = thinwire_protocol::AdapterEvent::Status {
            protocol: thinwire_protocol::ProtocolId::Signal,
            status: thinwire_protocol::AdapterStatus::Stubbed,
            detail: id.clone(),
        };
        let rendered = format!("{command:?} {event:?} {keys:?}");
        assert!(!rendered.contains(&secret), "{rendered}");
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
