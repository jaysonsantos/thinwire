//! Discord bot token and OAuth bearer token. Values stay in the secret vault.
//!
//! The vault is memory on the UI thread. OS keychain I/O stays in the desktop
//! crate on a `spawn_blocking` worker. Tokens never travel on [`crate::AdapterCommand`].

use std::fmt;
use std::sync::Mutex;

#[cfg(test)]
use super::super::adapter::AdapterCommand;

/// Keychain service for the Discord bot token. Same OS store as the rest of thinwire.
pub const DISCORD_SECRET_SERVICE: &str = "thinwire";
/// Keychain account for the Discord bot token or OAuth bearer token.
pub const DISCORD_SECRET_BOT_TOKEN: &str = "discord.bot_token";

/// How a stored Discord credential may be sent. User-account tokens are absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscordTokenKind {
    /// Bot token. `twilight_http::Client` adds the `Bot ` prefix when it is missing.
    Bot,
    /// OAuth bearer token for a bot install. Not a user access token.
    OAuthBearer,
}

/// Read a Discord bot or OAuth token without logging the value.
pub trait DiscordSecretVault: Send + Sync {
    fn bot_token(&self) -> Option<String>;
    fn set_bot_token(&self, value: &str);
}

/// Process-local vault for tests and headless runs.
pub struct MemoryDiscordVault {
    token: Mutex<Option<String>>,
}

impl MemoryDiscordVault {
    #[must_use]
    pub fn new() -> Self {
        Self {
            token: Mutex::new(None),
        }
    }
}

impl Default for MemoryDiscordVault {
    fn default() -> Self {
        Self::new()
    }
}

impl DiscordSecretVault for MemoryDiscordVault {
    fn bot_token(&self) -> Option<String> {
        self.token.lock().ok().and_then(|token| token.clone())
    }

    fn set_bot_token(&self, value: &str) {
        let Ok(mut token) = self.token.lock() else {
            return;
        };
        let trimmed = value.trim();
        if trimmed.is_empty() {
            *token = None;
        } else {
            *token = Some(trimmed.to_string());
        }
    }
}

impl fmt::Debug for MemoryDiscordVault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemoryDiscordVault")
            .field("bot_token", &"<redacted>")
            .finish()
    }
}

/// Classify a keychain value. `Err` means the value must not be sent to Discord.
pub fn classify_discord_token(raw: &str) -> Result<DiscordTokenKind, ()> {
    let token = raw.trim();
    if token.is_empty() || token.contains(['\n', '\r', '\0']) {
        return Err(());
    }
    let lower = token.to_ascii_lowercase();
    if lower.starts_with("mfa.") {
        return Err(());
    }
    let mut words = token.split_whitespace();
    let head = words.next().unwrap_or("").to_ascii_lowercase();
    let has_payload = words.next().is_some();
    if head == "user" || (head == "bearer" && !has_payload) {
        return Err(());
    }
    if head == "bearer" {
        return Ok(DiscordTokenKind::OAuthBearer);
    }
    Ok(DiscordTokenKind::Bot)
}

/// Token string passed to the bot HTTP client. Caller must already have classified it.
pub fn authorization_token(raw: &str) -> Result<String, ()> {
    let token = raw.trim();
    match classify_discord_token(token)? {
        DiscordTokenKind::Bot => Ok(token.to_string()),
        DiscordTokenKind::OAuthBearer => {
            let rest = token
                .split_once(' ')
                .map(|(_, rest)| rest.trim())
                .unwrap_or("");
            if rest.is_empty() {
                return Err(());
            }
            Ok(format!("Bearer {rest}"))
        }
    }
}

/// Commands carry a mode, never a token.
#[cfg(test)]
fn command_carries_discord_token(command: &AdapterCommand) -> bool {
    let rendered = format!("{command:?}").to_ascii_lowercase();
    rendered.contains("token") || rendered.contains("bearer") || rendered.contains("mfa.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_vault_redacts_debug() {
        let vault = MemoryDiscordVault::new();
        vault.set_bot_token("  fixture-bot-token  ");
        assert_eq!(vault.bot_token().as_deref(), Some("fixture-bot-token"));
        vault.set_bot_token("  ");
        assert_eq!(vault.bot_token(), None);
        vault.set_bot_token("fixture-bot-token");
        let debug = format!("{vault:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("fixture-bot-token"));
    }

    #[test]
    fn user_and_mfa_tokens_are_refused() {
        assert_eq!(
            classify_discord_token("fixture-bot-token"),
            Ok(DiscordTokenKind::Bot)
        );
        assert_eq!(
            classify_discord_token("Bot fixture-bot-token"),
            Ok(DiscordTokenKind::Bot)
        );
        assert_eq!(
            classify_discord_token("Bearer oauth-fixture"),
            Ok(DiscordTokenKind::OAuthBearer)
        );
        assert_eq!(
            authorization_token("bearer oauth-fixture").as_deref(),
            Ok("Bearer oauth-fixture")
        );
        assert!(classify_discord_token("User personal-token").is_err());
        assert!(classify_discord_token("mfa.personal-token").is_err());
        assert!(classify_discord_token("Bearer ").is_err());
        assert!(classify_discord_token("").is_err());
    }

    #[test]
    fn connect_command_has_no_token_payload() {
        let command = AdapterCommand::ConnectDiscord {
            mode: crate::DiscordAuthMode::Bot,
        };
        assert!(!command_carries_discord_token(&command));
        let oauth = AdapterCommand::ConnectDiscord {
            mode: crate::DiscordAuthMode::OAuth,
        };
        assert!(!command_carries_discord_token(&oauth));
    }
}
