//! Slack client credentials and workspace tokens.
//!
//! Persistent keys are the OS keychain flush set (same service name as the
//! Telegram store). Ephemeral OAuth `code` and `state` stay in the process
//! map. Values never travel on [`crate::AdapterCommand`] and are never logged.

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

use crate::secrets::TELEGRAM_SECRET_SERVICE;

/// OS keychain service. Shared with Telegram entries under `thinwire`.
pub const SLACK_SECRET_SERVICE: &str = TELEGRAM_SECRET_SERVICE;

pub const SLACK_SECRET_CLIENT_ID: &str = "slack.client_id";
pub const SLACK_SECRET_CLIENT_SECRET: &str = "slack.client_secret";
pub const SLACK_SECRET_APP_TOKEN: &str = "slack.app_token";
pub const SLACK_SECRET_BOT_TOKEN: &str = "slack.bot_token";
pub const SLACK_SECRET_TEAM_ID: &str = "slack.team_id";
pub const SLACK_SECRET_APP_ID: &str = "slack.app_id";
pub const SLACK_SECRET_OAUTH_CODE: &str = "slack.oauth_code";
pub const SLACK_SECRET_OAUTH_STATE: &str = "slack.oauth_state";

/// Slack secrets the future install worker and the keychain share.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum SlackSecretKey {
    ClientId,
    ClientSecret,
    /// Socket Mode app-level token (`connections:write`). Publisher or keychain.
    AppToken,
    /// Workspace install bot token from `oauth.v2.access`. Never a user token.
    BotToken,
    TeamId,
    /// Slack app id (`A…`) from the install. Not the client id.
    AppId,
    OAuthCode,
    OAuthState,
}

impl SlackSecretKey {
    pub const ALL: [Self; 8] = [
        Self::ClientId,
        Self::ClientSecret,
        Self::AppToken,
        Self::BotToken,
        Self::TeamId,
        Self::AppId,
        Self::OAuthCode,
        Self::OAuthState,
    ];

    /// Keys that may be flushed to the OS keychain.
    pub const PERSISTENT: [Self; 6] = [
        Self::ClientId,
        Self::ClientSecret,
        Self::AppToken,
        Self::BotToken,
        Self::TeamId,
        Self::AppId,
    ];

    /// Keys that stay in the process map only.
    pub const EPHEMERAL: [Self; 2] = [Self::OAuthCode, Self::OAuthState];

    #[must_use]
    pub const fn account(self) -> &'static str {
        match self {
            Self::ClientId => SLACK_SECRET_CLIENT_ID,
            Self::ClientSecret => SLACK_SECRET_CLIENT_SECRET,
            Self::AppToken => SLACK_SECRET_APP_TOKEN,
            Self::BotToken => SLACK_SECRET_BOT_TOKEN,
            Self::TeamId => SLACK_SECRET_TEAM_ID,
            Self::AppId => SLACK_SECRET_APP_ID,
            Self::OAuthCode => SLACK_SECRET_OAUTH_CODE,
            Self::OAuthState => SLACK_SECRET_OAUTH_STATE,
        }
    }

    #[must_use]
    pub const fn persist_to_os(self) -> bool {
        matches!(
            self,
            Self::ClientId
                | Self::ClientSecret
                | Self::AppToken
                | Self::BotToken
                | Self::TeamId
                | Self::AppId
        )
    }
}

impl fmt::Debug for SlackSecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match *self {
            Self::ClientId => "ClientId",
            Self::ClientSecret => "ClientSecret",
            Self::AppToken => "AppToken",
            Self::BotToken => "BotToken",
            Self::TeamId => "TeamId",
            Self::AppId => "AppId",
            Self::OAuthCode => "OAuthCode",
            Self::OAuthState => "OAuthState",
        })
    }
}

/// Read/write Slack secrets without logging values.
pub trait SlackSecretVault: Send + Sync {
    fn get_secret(&self, key: SlackSecretKey) -> Option<String>;
    fn set_secret(&self, key: SlackSecretKey, value: &str);
}

/// In-memory vault used by tests and as the stand-in until a worker flush exists.
pub struct MemorySlackVault {
    values: Mutex<HashMap<SlackSecretKey, String>>,
}

impl MemorySlackVault {
    #[must_use]
    pub fn new() -> Self {
        Self {
            values: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for MemorySlackVault {
    fn default() -> Self {
        Self::new()
    }
}

impl SlackSecretVault for MemorySlackVault {
    fn get_secret(&self, key: SlackSecretKey) -> Option<String> {
        self.values
            .lock()
            .ok()
            .and_then(|values| values.get(&key).cloned())
    }

    fn set_secret(&self, key: SlackSecretKey, value: &str) {
        let Ok(mut values) = self.values.lock() else {
            return;
        };
        let trimmed = value.trim();
        if trimmed.is_empty() {
            values.remove(&key);
        } else {
            values.insert(key, trimmed.to_string());
        }
    }
}

impl fmt::Debug for MemorySlackVault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemorySlackVault")
            .field("secrets", &"<redacted>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persistent_keys_are_the_os_flush_set() {
        assert_eq!(SLACK_SECRET_SERVICE, "thinwire");
        assert_eq!(SLACK_SECRET_SERVICE, TELEGRAM_SECRET_SERVICE);
        for key in SlackSecretKey::PERSISTENT {
            assert!(key.persist_to_os(), "{key:?}");
        }
        for key in SlackSecretKey::EPHEMERAL {
            assert!(!key.persist_to_os(), "{key:?}");
        }
        assert_eq!(SlackSecretKey::ALL.len(), 8);
        assert!(SlackSecretKey::BotToken.persist_to_os());
        assert!(!SlackSecretKey::OAuthCode.persist_to_os());
        let accounts: Vec<_> = SlackSecretKey::ALL
            .iter()
            .map(|key| key.account())
            .collect();
        assert!(accounts.contains(&"slack.client_id"));
        assert!(accounts.contains(&"slack.bot_token"));
        assert!(!accounts.iter().any(|account| account.contains("user")));
    }

    #[test]
    fn memory_vault_round_trip_redacts_debug() {
        let vault = MemorySlackVault::new();
        vault.set_secret(SlackSecretKey::ClientId, "  client-id-test  ");
        vault.set_secret(SlackSecretKey::ClientSecret, "client-secret-test");
        vault.set_secret(SlackSecretKey::AppToken, "app-token-test");
        vault.set_secret(SlackSecretKey::BotToken, "bot-token-test");
        vault.set_secret(SlackSecretKey::OAuthCode, "oauth-code-test");
        assert_eq!(
            vault.get_secret(SlackSecretKey::ClientId).as_deref(),
            Some("client-id-test")
        );
        vault.set_secret(SlackSecretKey::OAuthCode, "  ");
        assert_eq!(vault.get_secret(SlackSecretKey::OAuthCode), None);
        let debug = format!("{vault:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("client-id-test"));
        assert!(!debug.contains("client-secret-test"));
        assert!(!debug.contains("app-token-test"));
        assert!(!debug.contains("bot-token-test"));
        assert!(!debug.contains("oauth-code-test"));
    }
}
