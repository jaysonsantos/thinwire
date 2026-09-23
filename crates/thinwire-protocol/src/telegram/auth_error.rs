//! Map TDLib login errors to [`TelegramAuthError`]. No TDLib types, no values.
//!
//! TDLib reports a login failure as a numeric code and an error name, for
//! example `400 PHONE_CODE_INVALID` or `429 Too Many Requests: retry after 30`.
//! Only the variant (and a wait in seconds) crosses the channel.

#![cfg_attr(not(feature = "telegram-tdlib"), allow(dead_code))]

use crate::adapter::TelegramAuthError;

const FLOOD_WAIT_PREFIX: &str = "FLOOD_WAIT_";
const RETRY_AFTER: &str = "retry after ";
const TOO_MANY_REQUESTS: i32 = 429;

#[must_use]
pub(super) fn auth_error_from_tdlib(code: i32, message: &str) -> TelegramAuthError {
    let name = message.trim();
    match name {
        "PHONE_NUMBER_INVALID" => return TelegramAuthError::PhoneInvalid,
        "PHONE_CODE_INVALID" | "PHONE_CODE_EMPTY" => return TelegramAuthError::CodeInvalid,
        "PHONE_CODE_EXPIRED" => return TelegramAuthError::CodeExpired,
        "PASSWORD_HASH_INVALID" => return TelegramAuthError::PasswordInvalid,
        _ => {}
    }
    if let Some(seconds) = name
        .strip_prefix(FLOOD_WAIT_PREFIX)
        .and_then(|rest| rest.parse().ok())
    {
        return TelegramAuthError::FloodWait { seconds };
    }
    if code == TOO_MANY_REQUESTS
        && let Some(seconds) = name
            .find(RETRY_AFTER)
            .and_then(|at| name[at + RETRY_AFTER.len()..].trim().parse().ok())
    {
        return TelegramAuthError::FloodWait { seconds };
    }
    TelegramAuthError::Other { code }
}

/// TDLib error text that is safe to log: a bare error name (`A-Z0-9_`, for
/// example `PHONE_CODE_INVALID`) or text with no digits. Any other text can
/// hold a phone number or a login code, so it is never logged.
#[must_use]
pub(super) fn loggable_tdlib_message(message: &str) -> Option<&str> {
    let message = message.trim();
    if message.is_empty() {
        return None;
    }
    let bare_name = message
        .chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    let no_digits = !message.chars().any(|c| c.is_ascii_digit());
    (bare_name || no_digits).then_some(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_tdlib_names_map_to_specific_errors() {
        let cases = [
            (400, "PHONE_NUMBER_INVALID", TelegramAuthError::PhoneInvalid),
            (400, "PHONE_CODE_INVALID", TelegramAuthError::CodeInvalid),
            (400, "PHONE_CODE_EMPTY", TelegramAuthError::CodeInvalid),
            (400, "PHONE_CODE_EXPIRED", TelegramAuthError::CodeExpired),
            (
                400,
                "PASSWORD_HASH_INVALID",
                TelegramAuthError::PasswordInvalid,
            ),
            (
                420,
                "FLOOD_WAIT_120",
                TelegramAuthError::FloodWait { seconds: 120 },
            ),
            (
                429,
                "Too Many Requests: retry after 30",
                TelegramAuthError::FloodWait { seconds: 30 },
            ),
            (
                400,
                "PHONE_NUMBER_BANNED",
                TelegramAuthError::Other { code: 400 },
            ),
            (500, "", TelegramAuthError::Other { code: 500 }),
            (400, "FLOOD_WAIT_x", TelegramAuthError::Other { code: 400 }),
        ];
        for (code, message, expected) in cases {
            assert_eq!(auth_error_from_tdlib(code, message), expected, "{message}");
        }
    }

    #[test]
    fn only_names_and_digit_free_text_may_be_logged() {
        assert_eq!(
            loggable_tdlib_message("PHONE_CODE_INVALID"),
            Some("PHONE_CODE_INVALID")
        );
        assert_eq!(
            loggable_tdlib_message("FLOOD_WAIT_120"),
            Some("FLOOD_WAIT_120")
        );
        assert_eq!(
            loggable_tdlib_message(
                "Initialization parameters are needed: call setTdlibParameters first"
            ),
            Some("Initialization parameters are needed: call setTdlibParameters first")
        );
        assert_eq!(loggable_tdlib_message("+15551234567 is not valid"), None);
        assert_eq!(loggable_tdlib_message("code 12345 expired"), None);
        assert_eq!(
            loggable_tdlib_message("Too Many Requests: retry after 30"),
            None
        );
        assert_eq!(loggable_tdlib_message("  "), None);
    }

    #[test]
    fn unknown_messages_keep_only_the_code() {
        let error = auth_error_from_tdlib(400, "+15551234567 is not valid");
        assert_eq!(error, TelegramAuthError::Other { code: 400 });
        assert!(!format!("{error:?}").contains("5551234567"));
    }
}
