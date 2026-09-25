// SPDX-License-Identifier: AGPL-3.0-only
#![cfg_attr(not(feature = "signal-local"), allow(dead_code))]
//! Signal group v2 threads. The conversation id is `signal:group:` plus the master key.

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

/// Where an outbound conversation id goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OutboundTarget {
    Contact,
    Group([u8; 32]),
}

/// A group id decodes to 32 master-key bytes. Any other id is a contact.
///
/// # Errors
///
/// A `signal:group:` id that is not a 32-byte master key.
pub(crate) fn outbound_target(conversation_id: &str) -> Result<OutboundTarget, ()> {
    let Some(encoded) = conversation_id.strip_prefix(GROUP_PREFIX) else {
        return Ok(OutboundTarget::Contact);
    };
    let bytes = decode_base64(encoded).ok_or(())?;
    let key: [u8; 32] = bytes.try_into().map_err(|_| ())?;
    Ok(OutboundTarget::Group(key))
}

#[must_use]
pub(crate) fn group_id(master_key: &[u8]) -> String {
    format!("{GROUP_PREFIX}{}", encode_base64(master_key))
}

fn encode_base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    let mut index = 0;
    while index + 3 <= data.len() {
        let chunk = ((data[index] as u32) << 16)
            | ((data[index + 1] as u32) << 8)
            | (data[index + 2] as u32);
        push_digit(&mut out, (chunk >> 18) & 63, ALPHABET);
        push_digit(&mut out, (chunk >> 12) & 63, ALPHABET);
        push_digit(&mut out, (chunk >> 6) & 63, ALPHABET);
        push_digit(&mut out, chunk & 63, ALPHABET);
        index += 3;
    }
    match data.len() - index {
        1 => {
            let chunk = (data[index] as u32) << 16;
            push_digit(&mut out, (chunk >> 18) & 63, ALPHABET);
            push_digit(&mut out, (chunk >> 12) & 63, ALPHABET);
            out.push('=');
            out.push('=');
        }
        2 => {
            let chunk = ((data[index] as u32) << 16) | ((data[index + 1] as u32) << 8);
            push_digit(&mut out, (chunk >> 18) & 63, ALPHABET);
            push_digit(&mut out, (chunk >> 12) & 63, ALPHABET);
            push_digit(&mut out, (chunk >> 6) & 63, ALPHABET);
            out.push('=');
        }
        _ => {}
    }
    out
}

fn push_digit(out: &mut String, value: u32, alphabet: &[u8; 64]) {
    out.push(alphabet[value as usize] as char);
}

fn decode_base64(input: &str) -> Option<Vec<u8>> {
    let bytes = input.as_bytes();
    if bytes.is_empty() || !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let mut index = 0;
    while index < bytes.len() {
        let third = bytes[index + 2];
        let fourth = bytes[index + 3];
        let a = decode_digit(bytes[index])?;
        let b = decode_digit(bytes[index + 1])?;
        if third == b'=' {
            if fourth != b'=' {
                return None;
            }
            out.push((a << 2) | (b >> 4));
        } else if fourth == b'=' {
            let c = decode_digit(third)?;
            out.push((a << 2) | (b >> 4));
            out.push((b << 4) | (c >> 2));
        } else {
            let c = decode_digit(third)?;
            let d = decode_digit(fourth)?;
            out.push((a << 2) | (b >> 4));
            out.push((b << 4) | (c >> 2));
            out.push((c << 6) | d);
        }
        index += 4;
    }
    Some(out)
}

fn decode_digit(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
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
    fn a_group_id_is_the_master_key_and_not_a_contact() {
        let key = [0x11; 32];
        let id = group_id(&key);
        assert_eq!(outbound_target(&id), Ok(OutboundTarget::Group(key)));
        assert_eq!(
            outbound_target("11111111-1111-1111-1111-111111111111"),
            Ok(OutboundTarget::Contact)
        );
        assert!(outbound_target("signal:group:qq").is_err());
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
