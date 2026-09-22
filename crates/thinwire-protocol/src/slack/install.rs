//! Workspace-app OAuth v2 install shape.
//!
//! Builds the official authorize URL and parses a loopback callback query.
//! This module does not bind a socket and does not call Slack. The listener
//! and `oauth.v2.access` exchange belong on a tokio worker in a later beat.
//! See `decisions/0008-slack-oauth-workspace-spike.md`.

use std::fmt;

use super::secrets::{SlackSecretKey, SlackSecretVault};

/// Fixed loopback port. Register this redirect on the Slack app.
pub const SLACK_OAUTH_LOOPBACK_PORT: u16 = 8976;

/// Path Slack redirects to after the workspace admin approves the install.
pub const SLACK_OAUTH_CALLBACK_PATH: &str = "/slack/oauth/callback";

/// Bot scopes for a workspace app. No user scopes: that would be a personal token.
pub const WORKSPACE_BOT_SCOPES: &[&str] = &[
    "channels:history",
    "channels:read",
    "chat:write",
    "groups:history",
    "groups:read",
    "im:history",
    "im:read",
    "mpim:history",
    "mpim:read",
    "users:read",
];

const AUTHORIZE_URL: &str = "https://slack.com/oauth/v2/authorize";

/// Why a loopback callback was rejected. Display text never includes the query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlackCallbackError {
    Declined,
    Malformed,
    MissingCode,
    MissingState,
    StateMismatch,
}

impl fmt::Display for SlackCallbackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Declined => "slack workspace install was declined",
            Self::Malformed => "slack oauth callback query is malformed",
            Self::MissingCode => "slack oauth callback is missing the code",
            Self::MissingState => "slack oauth callback is missing the state",
            Self::StateMismatch => "slack oauth state did not match",
        })
    }
}

impl std::error::Error for SlackCallbackError {}

/// Metadata for one workspace install. The bot token stays in the vault.
#[derive(Clone, PartialEq, Eq)]
pub struct SlackInstalledWorkspace {
    team_id: String,
    team_name: String,
    bot_user_id: String,
    app_id: String,
}

impl SlackInstalledWorkspace {
    #[must_use]
    pub fn new(team_id: &str, team_name: &str, bot_user_id: &str, app_id: &str) -> Self {
        Self {
            team_id: team_id.trim().to_string(),
            team_name: team_name.trim().to_string(),
            bot_user_id: bot_user_id.trim().to_string(),
            app_id: app_id.trim().to_string(),
        }
    }

    #[must_use]
    pub fn team_id(&self) -> &str {
        &self.team_id
    }

    #[must_use]
    pub fn team_name(&self) -> &str {
        &self.team_name
    }

    #[must_use]
    pub fn bot_user_id(&self) -> &str {
        &self.bot_user_id
    }

    #[must_use]
    pub fn app_id(&self) -> &str {
        &self.app_id
    }

    /// Store the bot token and team id. Clear the one-time OAuth code and state.
    pub fn remember(&self, vault: &dyn SlackSecretVault, bot_token: &str) {
        vault.set_secret(SlackSecretKey::BotToken, bot_token);
        vault.set_secret(SlackSecretKey::TeamId, &self.team_id);
        vault.set_secret(SlackSecretKey::OAuthCode, "");
        vault.set_secret(SlackSecretKey::OAuthState, "");
    }
}

impl fmt::Debug for SlackInstalledWorkspace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SlackInstalledWorkspace")
            .field("team_id", &self.team_id)
            .field("team_name", &self.team_name)
            .field("bot_user_id", &self.bot_user_id)
            .field("app_id", &self.app_id)
            .finish()
    }
}

#[must_use]
pub fn loopback_redirect_uri() -> String {
    format!("http://127.0.0.1:{SLACK_OAUTH_LOOPBACK_PORT}{SLACK_OAUTH_CALLBACK_PATH}")
}

/// 16 CSPRNG bytes, hex-encoded. The value is an OAuth `state`, not a token.
#[must_use]
pub fn new_oauth_state() -> String {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("slack oauth state requires OS CSPRNG");
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(32);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Official Slack OAuth v2 authorize URL. The client secret is not a parameter.
#[must_use]
pub fn authorize_url(client_id: &str, state: &str) -> String {
    let scope = WORKSPACE_BOT_SCOPES.join(",");
    let mut url = String::from(AUTHORIZE_URL);
    url.push_str("?client_id=");
    push_query(&mut url, client_id);
    url.push_str("&scope=");
    push_query(&mut url, &scope);
    url.push_str("&redirect_uri=");
    push_query(&mut url, &loopback_redirect_uri());
    url.push_str("&state=");
    push_query(&mut url, state);
    url
}

/// Read `code` from a loopback callback query when `state` matches.
///
/// `raw_query` is the query string with or without a leading `?`. On success
/// the code is returned to the caller, which should put it in
/// [`SlackSecretKey::OAuthCode`] and exchange it off the UI thread.
///
/// `state` is checked before a decline is accepted. An `error` parameter
/// whose `state` is missing or different from `expected_state` is not a
/// decline of this install.
pub fn parse_loopback_callback(
    raw_query: &str,
    expected_state: &str,
) -> Result<String, SlackCallbackError> {
    let query = raw_query.trim().trim_start_matches('?');
    if query.is_empty() {
        return Err(SlackCallbackError::MissingCode);
    }
    let mut code = None;
    let mut state = None;
    let mut declined = false;
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (name, value) = pair.split_once('=').ok_or(SlackCallbackError::Malformed)?;
        let name = decode_component(name)?;
        let value = decode_component(value)?;
        match name.as_str() {
            "error" if !value.is_empty() => declined = true,
            "code" => code = Some(value),
            "state" => state = Some(value),
            _ => {}
        }
    }
    let Some(state) = state.filter(|value| !value.is_empty()) else {
        return Err(SlackCallbackError::MissingState);
    };
    if state != expected_state {
        return Err(SlackCallbackError::StateMismatch);
    }
    if declined {
        return Err(SlackCallbackError::Declined);
    }
    let Some(code) = code.filter(|value| !value.is_empty()) else {
        return Err(SlackCallbackError::MissingCode);
    };
    Ok(code)
}

fn push_query(url: &mut String, value: &str) {
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                url.push(byte as char);
            }
            _ => {
                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                url.push('%');
                url.push(HEX[(byte >> 4) as usize] as char);
                url.push(HEX[(byte & 0x0f) as usize] as char);
            }
        }
    }
}

fn decode_component(value: &str) -> Result<String, SlackCallbackError> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                if index + 2 >= bytes.len() {
                    return Err(SlackCallbackError::Malformed);
                }
                let hi = from_hex(bytes[index + 1])?;
                let lo = from_hex(bytes[index + 2])?;
                out.push((hi << 4) | lo);
                index += 3;
            }
            other => {
                out.push(other);
                index += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| SlackCallbackError::Malformed)
}

fn from_hex(byte: u8) -> Result<u8, SlackCallbackError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(SlackCallbackError::Malformed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slack::MemorySlackVault;

    #[test]
    fn authorize_url_is_workspace_install_without_secret() {
        let url = authorize_url("client-id-test", "state-test");
        assert!(url.starts_with("https://slack.com/oauth/v2/authorize?"));
        assert!(url.contains("client_id=client-id-test"));
        assert!(url.contains("state=state-test"));
        assert!(url.contains("scope="));
        assert!(url.contains("chat%3Awrite"));
        assert!(url.contains("redirect_uri="));
        assert!(url.contains("127.0.0.1"));
        assert!(url.contains("8976"));
        assert!(url.contains("%2Fslack%2Foauth%2Fcallback"));
        assert!(!url.contains("user_scope"));
        assert!(!url.contains("client_secret"));
        assert!(!url.contains("client-secret-test"));
        assert_eq!(
            loopback_redirect_uri(),
            "http://127.0.0.1:8976/slack/oauth/callback"
        );
    }

    #[test]
    fn callback_returns_code_only_when_state_matches() {
        let code = parse_loopback_callback("code=oauth-code-test&state=state-test", "state-test")
            .expect("callback");
        assert_eq!(code, "oauth-code-test");
        let prefixed =
            parse_loopback_callback("?code=oauth-code-test&state=state-test", "state-test")
                .expect("prefixed");
        assert_eq!(prefixed, "oauth-code-test");
    }

    #[test]
    fn callback_errors_omit_query_secrets() {
        let mismatch = parse_loopback_callback("code=oauth-code-test&state=other", "state-test")
            .expect_err("mismatch");
        assert_eq!(mismatch, SlackCallbackError::StateMismatch);
        let shown = mismatch.to_string();
        assert!(!shown.contains("oauth-code-test"));
        assert!(!shown.contains("other"));
        assert!(!shown.contains("state-test"));

        let declined =
            parse_loopback_callback("error=access_denied&state=state-test", "state-test")
                .expect_err("declined");
        assert_eq!(declined, SlackCallbackError::Declined);
        assert!(!declined.to_string().contains("access_denied"));
    }

    #[test]
    fn forged_decline_without_matching_state_is_not_accepted() {
        let missing = parse_loopback_callback("?error=access_denied", "state-test")
            .expect_err("unbound decline");
        assert_eq!(missing, SlackCallbackError::MissingState);
        assert_ne!(missing, SlackCallbackError::Declined);

        let forged = parse_loopback_callback("error=access_denied&state=other-state", "state-test")
            .expect_err("forged decline");
        assert_eq!(forged, SlackCallbackError::StateMismatch);
        assert_ne!(forged, SlackCallbackError::Declined);
        let shown = forged.to_string();
        assert!(!shown.contains("access_denied"));
        assert!(!shown.contains("other-state"));
        assert!(!shown.contains("state-test"));
    }

    #[test]
    fn remember_stores_bot_token_and_clears_ephemeral_code() {
        let vault = MemorySlackVault::new();
        vault.set_secret(SlackSecretKey::OAuthCode, "oauth-code-test");
        vault.set_secret(SlackSecretKey::OAuthState, "state-test");
        let install = SlackInstalledWorkspace::new("T-test", "Workspace", "B-test", "A-test");
        install.remember(&vault, "bot-token-test");
        assert_eq!(
            vault.get_secret(SlackSecretKey::BotToken).as_deref(),
            Some("bot-token-test")
        );
        assert_eq!(
            vault.get_secret(SlackSecretKey::TeamId).as_deref(),
            Some("T-test")
        );
        assert_eq!(vault.get_secret(SlackSecretKey::OAuthCode), None);
        assert_eq!(vault.get_secret(SlackSecretKey::OAuthState), None);
        let debug = format!("{install:?}");
        assert!(debug.contains("T-test"));
        assert!(!debug.contains("bot-token-test"));
    }

    #[test]
    fn oauth_state_is_32_hex_chars() {
        let state = new_oauth_state();
        assert_eq!(state.len(), 32);
        assert!(state.chars().all(|ch| ch.is_ascii_hexdigit()));
        assert_ne!(state, new_oauth_state());
    }
}
