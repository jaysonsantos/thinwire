//! `slack-morphism` OAuth v2 request and Socket Mode config.
//!
//! Compiled only with `--features slack-oauth`. Constructs values. Does not
//! open a socket and does not call Slack; `live.rs` does that.

use slack_morphism::prelude::*;

use super::install::loopback_redirect_uri;

/// `oauth.v2.access` body for the loopback redirect. Caller supplies vault values.
#[must_use]
pub fn oauth_v2_access_request(
    client_id: &str,
    client_secret: &str,
    code: &str,
) -> SlackOAuthV2AccessTokenRequest {
    let redirect_uri = Some(
        loopback_redirect_uri()
            .parse()
            .expect("loopback redirect uri is a static http://127.0.0.1 URL"),
    );
    SlackOAuthV2AccessTokenRequest {
        client_id: client_id.into(),
        client_secret: client_secret.into(),
        code: code.into(),
        redirect_uri,
    }
}

/// Bot token for one workspace. User tokens are not represented.
#[must_use]
pub fn workspace_bot_token(bot_token: &str, team_id: &str) -> SlackApiToken {
    let mut token = SlackApiToken::new(bot_token.into());
    token.token_type = Some(SlackApiTokenType::Bot);
    token.team_id = Some(team_id.into());
    token
}

/// Socket Mode client settings. `live.rs` passes the app-level token.
#[must_use]
pub fn socket_mode_config() -> SlackClientSocketModeConfig {
    SlackClientSocketModeConfig::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_request_uses_loopback_and_keeps_secret_off_the_redirect() {
        let request = oauth_v2_access_request("client-id-test", "client-secret-test", "code-test");
        let redirect = request.redirect_uri.as_ref().expect("redirect").as_str();
        assert_eq!(redirect, "http://127.0.0.1:8976/slack/oauth/callback");
        assert!(!redirect.contains("client-secret-test"));
        assert!(!redirect.contains("code-test"));
        assert_eq!(request.client_id.value(), "client-id-test");
        assert_eq!(request.client_secret.value(), "client-secret-test");
        assert_eq!(request.code.value(), "code-test");
        let debug = format!("{request:?}");
        assert!(!debug.contains("client-secret-test"));
        assert!(!debug.contains("code-test"));
    }

    #[test]
    fn bot_token_is_a_workspace_bot_not_a_user_token() {
        let token = workspace_bot_token("bot-token-test", "T-test");
        assert_eq!(token.token_type, Some(SlackApiTokenType::Bot));
        assert_eq!(token.token_value.value(), "bot-token-test");
        assert_eq!(
            token.team_id.as_ref().map(|id| id.value().as_str()),
            Some("T-test")
        );
        let debug = format!("{:?}", token.token_value);
        assert!(!debug.contains("bot-token-test"));
    }

    #[test]
    fn socket_mode_config_uses_library_defaults() {
        let config = socket_mode_config();
        assert_eq!(
            config.max_connections_count,
            SlackClientSocketModeConfig::DEFAULT_CONNECTIONS_COUNT
        );
        assert!(!config.debug_connections);
    }
}
