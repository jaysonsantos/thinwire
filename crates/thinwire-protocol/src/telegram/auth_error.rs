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

/// Known symbolic TDLib / Telegram error names. Only these names are ever
/// logged. Any other text is redacted: free text can hold a phone number, a
/// login code (digits, or an SMS word or phrase), or a 2FA password.
const LOGGABLE_ERROR_NAMES: &[&str] = &[
    "API_ID_INVALID",
    "API_ID_PUBLISHED_FLOOD",
    "AUTH_KEY_UNREGISTERED",
    "PASSWORD_HASH_INVALID",
    "PHONE_CODE_EMPTY",
    "PHONE_CODE_EXPIRED",
    "PHONE_CODE_INVALID",
    "PHONE_NUMBER_BANNED",
    "PHONE_NUMBER_FLOOD",
    "PHONE_NUMBER_INVALID",
    "PHONE_PASSWORD_FLOOD",
    "SESSION_PASSWORD_NEEDED",
];

/// TDLib error text that is safe to log: an exact name from
/// [`LOGGABLE_ERROR_NAMES`], or `FLOOD_WAIT_` followed by digits only.
/// Everything else is redacted, with or without digits.
#[must_use]
pub(super) fn loggable_tdlib_message(message: &str) -> Option<&str> {
    let message = message.trim();
    if LOGGABLE_ERROR_NAMES.contains(&message) {
        return Some(message);
    }
    let flood_wait = message
        .strip_prefix(FLOOD_WAIT_PREFIX)
        .is_some_and(|seconds| !seconds.is_empty() && seconds.chars().all(|c| c.is_ascii_digit()));
    flood_wait.then_some(message)
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
    fn only_known_error_names_may_be_logged() {
        assert_eq!(
            loggable_tdlib_message("PHONE_CODE_INVALID"),
            Some("PHONE_CODE_INVALID")
        );
        assert_eq!(
            loggable_tdlib_message(" FLOOD_WAIT_120 "),
            Some("FLOOD_WAIT_120")
        );
        for secret_or_free_text in [
            // Free text, with or without digits (Codex 4091477235).
            "Initialization parameters are needed: call setTdlibParameters first",
            "Wrong database encryption key",
            "+15551234567 is not valid",
            "code 12345 expired",
            "Too Many Requests: retry after 30",
            // Values a user types: none of them may reach a log.
            "15551234567",
            "12345",
            "_12345",
            "apple banana",
            "correcthorsebattery",
            "CORRECTHORSE",
            "MY_SECRET_PASSWORD",
            "FLOOD_WAIT_",
            "FLOOD_WAIT_12a",
            "phone_code_invalid",
            "",
        ] {
            assert_eq!(
                loggable_tdlib_message(secret_or_free_text),
                None,
                "{secret_or_free_text:?} must be redacted"
            );
        }
    }

    #[test]
    fn unknown_messages_keep_only_the_code() {
        let error = auth_error_from_tdlib(400, "+15551234567 is not valid");
        assert_eq!(error, TelegramAuthError::Other { code: 400 });
        assert!(!format!("{error:?}").contains("5551234567"));
    }
}
