//! Light and dark preference. Missing config is System (ADR 0005).

use std::fmt;

/// User override for light/dark. Default and missing config are System.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeMode {
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeMode {
    /// Every mode in picker order.
    pub const ALL: [Self; 3] = [Self::System, Self::Light, Self::Dark];

    /// Value written to `settings.toml`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    /// Parse a `settings.toml` value. Quotes, spaces, and case are ignored.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().trim_matches('"').to_ascii_lowercase().as_str() {
            "system" => Some(Self::System),
            "light" => Some(Self::Light),
            "dark" => Some(Self::Dark),
            _ => None,
        }
    }
}

impl fmt::Display for ThemeMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_quoted_and_mixed_case() {
        assert_eq!(ThemeMode::parse(" System "), Some(ThemeMode::System));
        assert_eq!(ThemeMode::parse("\"DARK\""), Some(ThemeMode::Dark));
        assert_eq!(ThemeMode::parse("light"), Some(ThemeMode::Light));
        assert_eq!(ThemeMode::parse("nope"), None);
    }

    #[test]
    fn as_str_round_trips() {
        for mode in ThemeMode::ALL {
            assert_eq!(ThemeMode::parse(mode.as_str()), Some(mode));
        }
    }
}
