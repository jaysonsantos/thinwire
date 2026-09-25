#![cfg_attr(not(feature = "signal-local"), allow(dead_code))]
//! Signal group v2 threads. The id is the master key, hex encoded.

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

#[must_use]
pub(crate) fn group_id(master_key: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(master_key.len() * 2);
    for byte in master_key {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
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
        assert_eq!(chat.id, "ab0c11ff");
        assert_eq!(group_chat(&key, "").title, "Signal group");
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
