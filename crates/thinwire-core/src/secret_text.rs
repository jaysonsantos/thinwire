//! Text that a user typed into a secret field. `Debug` never shows it.

use std::fmt;

/// Phone number, login code, password, or API value on its way to the core.
///
/// The core copies it into the memory vault. It never goes on an
/// `AdapterCommand` and never goes to a log.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct SecretText(String);

impl SecretText {
    /// Wrap a typed value.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The raw value. Callers must not log or persist it outside the vault.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// True when the value holds no text.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<String> for SecretText {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for SecretText {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
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
    fn debug_is_redacted() {
        let secret = SecretText::new("+15550100");
        let shown = format!("{secret:?}");
        assert!(!shown.contains("5550100"));
        assert_eq!(secret.expose(), "+15550100");
        assert!(SecretText::default().is_empty());
    }
}
