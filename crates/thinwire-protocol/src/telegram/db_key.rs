//! TDLib database encryption key. Always compiled so default CI can test
//! CSPRNG generation without linking TDLib.

/// 32 CSPRNG bytes, hex-encoded. Never log the return value.
/// Live `telegram-tdlib` reads this from `tdlib.rs`; default CI only tests it.
#[cfg_attr(not(feature = "telegram-tdlib"), allow(dead_code))]
#[must_use]
pub(super) fn generate_db_key() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("telegram db encryption key requires OS CSPRNG");
    hex_encode(&bytes)
}

#[cfg_attr(not(feature = "telegram-tdlib"), allow(dead_code))]
fn hex_encode(bytes: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_db_key_is_64_hex_chars() {
        let key = generate_db_key();
        assert_eq!(key.len(), 64);
        assert!(key.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(key, key.to_ascii_lowercase());
        let other = generate_db_key();
        assert_eq!(other.len(), 64);
        assert!(key != other, "CSPRNG produced a colliding 32-byte key");
    }
}
