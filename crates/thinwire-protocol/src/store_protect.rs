//! Sentences for a session store the helper could not protect.
//!
//! The account row shows the sentence. The chip keeps its short label.
//! The sentences do not name the cipher, the key, or a secret.

/// The keychain refused the read. The same sentence for Signal and WhatsApp.
pub const KEYCHAIN_ACCESS_DENIED: &str = "Can't read the session key from Keychain. Allow access";

/// The encrypted Signal store did not open. The old folder was kept aside.
#[must_use]
pub const fn signal_session_kept() -> &'static str {
    "Can't open the saved Signal session. Link again. Your old data is kept."
}

/// The encrypted WhatsApp store did not open. The old folder was kept aside.
#[must_use]
pub const fn whatsapp_session_kept() -> &'static str {
    "Can't open the saved WhatsApp session. Link again. Your old data is kept."
}

/// The keychain cannot keep the Signal key. The helper did not link.
#[must_use]
pub const fn signal_key_not_saved() -> &'static str {
    "The keychain cannot save the Signal session key. Signal did not link."
}

/// The keychain cannot keep the WhatsApp key. The helper did not link.
#[must_use]
pub const fn whatsapp_key_not_saved() -> &'static str {
    "The keychain cannot save the WhatsApp session key. WhatsApp did not link."
}

/// The keychain read failed. The helper did not change the store.
#[must_use]
pub const fn signal_keychain_unavailable() -> &'static str {
    "The keychain cannot be read. Signal did not link."
}

/// The keychain read failed. The helper did not change the store.
#[must_use]
pub const fn whatsapp_keychain_unavailable() -> &'static str {
    "The keychain cannot be read. WhatsApp did not link."
}

/// The encrypted store did not open. `folder` is the new folder name.
#[must_use]
pub fn signal_key_wrong(folder: &str) -> String {
    format!(
        "The Signal session key is missing or wrong. The old store was moved aside to {folder}."
    )
}

/// The encrypted store did not open. `folder` is the new folder name.
#[must_use]
pub fn whatsapp_key_wrong(folder: &str) -> String {
    format!(
        "The WhatsApp session key is missing or wrong. The old store was moved aside to {folder}."
    )
}

/// The folder rename failed. The helper did not delete the store.
#[must_use]
pub const fn signal_key_wrong_kept() -> &'static str {
    "The Signal session key is missing or wrong. The old store was kept because it could not be moved aside."
}

/// The folder rename failed. The helper did not delete the store.
#[must_use]
pub const fn whatsapp_key_wrong_kept() -> &'static str {
    "The WhatsApp session key is missing or wrong. The old store was kept because it could not be moved aside."
}

/// The export or the row check failed. The plaintext store is unchanged.
#[must_use]
pub const fn signal_encrypt_failed() -> &'static str {
    "The Signal session store could not be encrypted. The old store was kept."
}

/// The export or the row check failed. The plaintext store is unchanged.
#[must_use]
pub const fn whatsapp_encrypt_failed() -> &'static str {
    "The WhatsApp session store could not be encrypted. The old store was kept."
}

/// `true` when `detail` is a store-protection sentence.
///
/// The UI draws these lines under the account chip. Other status text stays
/// on the chip label only.
#[must_use]
pub fn is_store_protection_detail(detail: &str) -> bool {
    detail == KEYCHAIN_ACCESS_DENIED
        || detail == signal_session_kept()
        || detail == whatsapp_session_kept()
        || detail.starts_with("The keychain cannot save the ")
        || detail.starts_with("The Signal session store could not be encrypted.")
        || detail.starts_with("The WhatsApp session store could not be encrypted.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protection_sentences_match_the_account_row_prefixes() {
        let sentences = [
            KEYCHAIN_ACCESS_DENIED,
            signal_session_kept(),
            whatsapp_session_kept(),
            signal_key_not_saved(),
            whatsapp_key_not_saved(),
            signal_encrypt_failed(),
            whatsapp_encrypt_failed(),
        ];
        for sentence in sentences {
            assert!(is_store_protection_detail(sentence), "{sentence}");
        }
        assert!(!is_store_protection_detail("Signal is linked."));
        assert!(!is_store_protection_detail(""));
    }
}
