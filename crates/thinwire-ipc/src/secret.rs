//! Text that crosses the pipe and must not go to a log.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A phone number, a QR payload, or a pair code on the wire.
///
/// `Debug` never shows the value. Neither the app nor a helper logs it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretText(String);

impl SecretText {
    /// Wrap a value for the wire.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The raw value. Callers must not log or persist it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretText(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_is_redacted_and_the_wire_form_is_plain_text() {
        let secret = SecretText::new("15550100");
        assert_eq!(format!("{secret:?}"), "SecretText(<redacted>)");
        assert_eq!(
            serde_json::to_string(&secret).expect("json"),
            "\"15550100\""
        );
        assert_eq!(secret.expose(), "15550100");
    }
}
