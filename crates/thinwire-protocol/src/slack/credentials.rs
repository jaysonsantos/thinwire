//! Resolve Slack client id / secret / app-level token without putting them
//! on commands.
//!
//! Precedence: keychain override, then publisher inject from
//! `SLACK_CLIENT_ID` / `SLACK_CLIENT_SECRET` / `SLACK_APP_TOKEN` at compile
//! time. Neither value is logged or stored in the git tree. Public CI must
//! not set these.

use std::fmt;

use super::secrets::{SlackSecretKey, SlackSecretVault};
use crate::adapter::{AdapterError, ProtocolId};

/// Where a resolved Slack credential came from. Never carries the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlackApiOrigin {
    KeychainOverride,
    PublisherInject,
}

/// Compile-time publisher Slack app credentials, if an official build injected them.
#[derive(Clone)]
pub struct SlackApiSource {
    publisher_client_id: Option<String>,
    publisher_client_secret: Option<String>,
    publisher_app_token: Option<String>,
}

impl SlackApiSource {
    /// Read `option_env!("SLACK_CLIENT_ID")`, `SLACK_CLIENT_SECRET`, and
    /// `SLACK_APP_TOKEN`. Empty strings count as missing.
    #[must_use]
    pub fn from_build() -> Self {
        Self {
            publisher_client_id: injected("SLACK_CLIENT_ID"),
            publisher_client_secret: injected("SLACK_CLIENT_SECRET"),
            publisher_app_token: injected("SLACK_APP_TOKEN"),
        }
    }

    /// No publisher credentials. Dev / CI default unless env was injected.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            publisher_client_id: None,
            publisher_client_secret: None,
            publisher_app_token: None,
        }
    }

    /// Test helper. Callers must not use real production credentials.
    #[must_use]
    pub fn with_publisher(client_id: &str, client_secret: &str, app_token: &str) -> Self {
        let id = nonempty(client_id);
        let secret = nonempty(client_secret);
        if id.is_none() || secret.is_none() {
            return Self {
                publisher_client_id: None,
                publisher_client_secret: None,
                publisher_app_token: nonempty(app_token),
            };
        }
        Self {
            publisher_client_id: id,
            publisher_client_secret: secret,
            publisher_app_token: nonempty(app_token),
        }
    }

    #[must_use]
    pub fn has_publisher_client(&self) -> bool {
        self.publisher_client_id.is_some() && self.publisher_client_secret.is_some()
    }

    #[must_use]
    pub fn has_publisher_app_token(&self) -> bool {
        self.publisher_app_token.is_some()
    }

    fn publisher_client(&self) -> Option<(&str, &str)> {
        Some((
            self.publisher_client_id.as_deref()?,
            self.publisher_client_secret.as_deref()?,
        ))
    }
}

impl Default for SlackApiSource {
    fn default() -> Self {
        Self::from_build()
    }
}

impl fmt::Debug for SlackApiSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SlackApiSource")
            .field("publisher_client", &self.has_publisher_client())
            .field("publisher_app_token", &self.has_publisher_app_token())
            .finish()
    }
}

fn injected(name: &str) -> Option<String> {
    let value = match name {
        "SLACK_CLIENT_ID" => option_env!("SLACK_CLIENT_ID"),
        "SLACK_CLIENT_SECRET" => option_env!("SLACK_CLIENT_SECRET"),
        "SLACK_APP_TOKEN" => option_env!("SLACK_APP_TOKEN"),
        _ => None,
    };
    value.map(str::trim).and_then(nonempty)
}

fn nonempty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Keychain override wins. Publisher inject is second. Values are never logged.
#[must_use]
pub fn resolve_slack_client(
    vault: &dyn SlackSecretVault,
    source: &SlackApiSource,
) -> Option<(String, String, SlackApiOrigin)> {
    let override_id = vault.get_secret(SlackSecretKey::ClientId);
    let override_secret = vault.get_secret(SlackSecretKey::ClientSecret);
    if let (Some(id), Some(secret)) = (override_id, override_secret)
        && !id.is_empty()
        && !secret.is_empty()
    {
        return Some((id, secret, SlackApiOrigin::KeychainOverride));
    }
    source.publisher_client().map(|(id, secret)| {
        (
            id.to_string(),
            secret.to_string(),
            SlackApiOrigin::PublisherInject,
        )
    })
}

/// Socket Mode app-level token. Independent of the OAuth client pair.
#[must_use]
pub fn resolve_slack_app_token(
    vault: &dyn SlackSecretVault,
    source: &SlackApiSource,
) -> Option<(String, SlackApiOrigin)> {
    if let Some(token) = vault.get_secret(SlackSecretKey::AppToken)
        && !token.is_empty()
    {
        return Some((token, SlackApiOrigin::KeychainOverride));
    }
    source
        .publisher_app_token
        .as_deref()
        .map(|token| (token.to_string(), SlackApiOrigin::PublisherInject))
}

pub(super) fn require_slack_client(
    vault: &dyn SlackSecretVault,
    source: &SlackApiSource,
) -> Result<(String, String), AdapterError> {
    resolve_slack_client(vault, source)
        .map(|(id, secret, _)| (id, secret))
        .ok_or(AdapterError::Unavailable {
            protocol: ProtocolId::Slack,
            reason: "slack client credentials are missing; set a keychain override or rebuild with SLACK_CLIENT_ID",
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slack::MemorySlackVault;

    #[test]
    fn override_wins_over_publisher() {
        let vault = MemorySlackVault::new();
        vault.set_secret(SlackSecretKey::ClientId, "client-id-override");
        vault.set_secret(SlackSecretKey::ClientSecret, "client-secret-override");
        let source =
            SlackApiSource::with_publisher("client-id-pub", "client-secret-pub", "app-token-pub");
        let (id, secret, origin) = resolve_slack_client(&vault, &source).expect("pair");
        assert_eq!(id, "client-id-override");
        assert_eq!(secret, "client-secret-override");
        assert_eq!(origin, SlackApiOrigin::KeychainOverride);
        let debug = format!("{source:?} {origin:?}");
        assert!(debug.contains("publisher_client: true"));
        assert!(!debug.contains("client-id-pub"));
        assert!(!debug.contains("client-secret-pub"));
        assert!(!debug.contains("client-secret-override"));
        assert!(!debug.contains("app-token-pub"));
    }

    #[test]
    fn publisher_used_when_vault_empty() {
        let vault = MemorySlackVault::new();
        let source =
            SlackApiSource::with_publisher("client-id-pub", "client-secret-pub", "app-token-pub");
        let (id, secret, origin) = resolve_slack_client(&vault, &source).expect("pair");
        assert_eq!(id, "client-id-pub");
        assert_eq!(secret, "client-secret-pub");
        assert_eq!(origin, SlackApiOrigin::PublisherInject);
        let (app, app_origin) = resolve_slack_app_token(&vault, &source).expect("app token");
        assert_eq!(app, "app-token-pub");
        assert_eq!(app_origin, SlackApiOrigin::PublisherInject);
    }

    #[test]
    fn app_token_override_wins() {
        let vault = MemorySlackVault::new();
        vault.set_secret(SlackSecretKey::AppToken, "app-token-override");
        let source =
            SlackApiSource::with_publisher("client-id-pub", "client-secret-pub", "app-token-pub");
        let (app, origin) = resolve_slack_app_token(&vault, &source).expect("app token");
        assert_eq!(app, "app-token-override");
        assert_eq!(origin, SlackApiOrigin::KeychainOverride);
    }

    #[test]
    fn missing_both_is_none() {
        let vault = MemorySlackVault::new();
        let source = SlackApiSource::empty();
        assert!(resolve_slack_client(&vault, &source).is_none());
        assert!(resolve_slack_app_token(&vault, &source).is_none());
        assert!(!source.has_publisher_client());
        assert!(!source.has_publisher_app_token());
    }

    #[test]
    fn partial_override_does_not_mix_with_publisher() {
        let vault = MemorySlackVault::new();
        vault.set_secret(SlackSecretKey::ClientId, "client-id-override");
        let source =
            SlackApiSource::with_publisher("client-id-pub", "client-secret-pub", "app-token-pub");
        let (id, secret, origin) = resolve_slack_client(&vault, &source).expect("publisher pair");
        assert_eq!(origin, SlackApiOrigin::PublisherInject);
        assert_eq!(id, "client-id-pub");
        assert_eq!(secret, "client-secret-pub");
    }

    #[test]
    fn from_build_debug_never_prints_values() {
        let source = SlackApiSource::from_build();
        let debug = format!("{source:?}");
        assert!(debug.contains("SlackApiSource"));
        assert!(!debug.contains("SLACK_CLIENT"));
        assert!(!debug.contains("SLACK_APP_TOKEN"));
    }
}
