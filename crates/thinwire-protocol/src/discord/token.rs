//! Discord bot token. Values stay in the secret vault.
//!
//! The vault is memory on the UI thread. OS keychain I/O stays in the desktop
//! crate on a `spawn_blocking` worker. Tokens never travel on [`crate::AdapterCommand`].
//!
//! A `Bearer` prefix is not application or bot provenance and does not prove the
//! `bot` scope. Those values are refused. Twilight forwards a `Bearer ` token
//! unchanged, which is how a user OAuth access token would skip the user-token refusal.

use std::fmt;
use std::sync::Mutex;

#[cfg(test)]
use super::super::adapter::AdapterCommand;

/// Keychain service for the Discord bot token. Same OS store as the rest of thinwire.
pub const DISCORD_SECRET_SERVICE: &str = "thinwire";
/// Keychain account for the Discord bot token.
pub const DISCORD_SECRET_BOT_TOKEN: &str = "discord.bot_token";

/// Credential that may be handed to the bot HTTP client.
///
/// Only the bot authorization scheme is accepted here. OAuth bearer tokens need
/// a verified application id and the `bot` scope; this spike does not invent
/// that proof from the `Bearer` prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscordTokenKind {
    /// Bot token. `twilight_http::Client` adds the `Bot ` prefix when it is missing.
    Bot,
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
///
/// `Bearer` is refused even when the payload is nonempty. The prefix is not a
/// check of bot/application provenance or allowed scopes.
pub fn classify_discord_token(raw: &str) -> Result<DiscordTokenKind, ()> {
    let token = raw.trim();
    if token.is_empty() || token.contains(['\n', '\r', '\0']) {
        return Err(());
    }
    let (scheme, payload) = split_authorization_scheme(token);
    if scheme_is_user_credential(scheme) || payload_is_user_credential(payload) {
        return Err(());
    }
    Ok(DiscordTokenKind::Bot)
}

fn scheme_is_user_credential(scheme: Option<&str>) -> bool {
    matches!(scheme, Some("bearer") | Some("user"))
}

/// Token string passed to the bot HTTP client.
///
/// The returned value never uses the `Bearer` scheme. Twilight leaves a
/// `Bearer ` prefix in place, so returning one would send a user OAuth token.
pub fn authorization_token(raw: &str) -> Result<String, ()> {
    let token = raw.trim();
    classify_discord_token(token)?;
    let lower = token.to_ascii_lowercase();
    if lower.starts_with("bearer ") || lower.starts_with("bearer\t") {
        return Err(());
    }
    Ok(token.to_string())
}

/// Split a leading `Bot` / `Bearer` / `User` scheme from its payload.
fn split_authorization_scheme(token: &str) -> (Option<&'static str>, &str) {
    let Some(head) = token.split_whitespace().next() else {
        return (None, token);
    };
    let scheme = match head.to_ascii_lowercase().as_str() {
        "bot" => Some("bot"),
        "bearer" => Some("bearer"),
        "user" => Some("user"),
        _ => None,
    };
    let Some(scheme) = scheme else {
        return (None, token);
    };
    let payload = token
        .split_once(char::is_whitespace)
        .map(|(_, rest)| rest.trim())
        .unwrap_or("");
    (Some(scheme), payload)
}

/// User-account material, including when it is wrapped in a `Bot` prefix.
fn payload_is_user_credential(payload: &str) -> bool {
    let mut payload = payload.trim();
    loop {
        if payload.is_empty() || payload.to_ascii_lowercase().starts_with("mfa.") {
            return true;
        }
        let (scheme, inner) = split_authorization_scheme(payload);
        match scheme {
            Some("bearer") | Some("user") => return true,
            Some("bot") => payload = inner,
            Some(_) | None => return false,
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
            authorization_token("fixture-bot-token").as_deref(),
            Ok("fixture-bot-token")
        );
        assert_eq!(
            authorization_token("Bot fixture-bot-token").as_deref(),
            Ok("Bot fixture-bot-token")
        );
        for refused in [
            "Bearer oauth-fixture",
            "bearer oauth-fixture",
            "Bearer ",
            "User personal-token",
            "mfa.personal-token",
            "Bot mfa.personal-token",
            "Bot bearer oauth-fixture",
            "Bot user personal-token",
            "",
        ] {
            assert!(
                classify_discord_token(refused).is_err(),
                "{refused} must be refused"
            );
            assert!(
                authorization_token(refused).is_err(),
                "{refused} must not become an authorization value"
            );
        }
        assert!(
            authorization_token("Bearer oauth-fixture")
                .ok()
                .is_none_or(|value| !value.to_ascii_lowercase().starts_with("bearer"))
        );
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
