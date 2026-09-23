//! Slack official OAuth / workspace-app stub. Not a personal desktop clone.
//!
//! Feature `slack-oauth` compiles `slack-morphism` types. Default builds keep
//! the feature off. This adapter does not open a network session.

mod credentials;
mod install;
#[cfg(feature = "slack-oauth")]
mod morphism;
mod secrets;

use credentials::require_slack_client;

pub use credentials::{
    SlackApiOrigin, SlackApiSource, resolve_slack_app_token, resolve_slack_client,
};
pub use install::{
    SLACK_OAUTH_CALLBACK_PATH, SLACK_OAUTH_LOOPBACK_PORT, SlackCallbackError,
    SlackInstalledWorkspace, WORKSPACE_BOT_SCOPES, authorize_url, loopback_redirect_uri,
    new_oauth_state, parse_loopback_callback,
};
#[cfg(feature = "slack-oauth")]
pub use morphism::{oauth_v2_access_request, socket_mode_config, workspace_bot_token};
pub use secrets::{MemorySlackVault, SLACK_SECRET_SERVICE, SlackSecretKey, SlackSecretVault};

use super::adapter::{
    AdapterCommand, AdapterError, AdapterStatus, ChatMessage, Conversation, Delivery, EventTx,
    ProtocolAdapter, ProtocolCapabilities, ProtocolId, SupportClass, emit_conversation,
    emit_message, emit_status,
};

#[cfg(feature = "slack-oauth")]
const CAPABILITY_DETAIL: &str = "Official Slack OAuth / workspace app (slack-morphism). Supported goal. Not a personal desktop clone. OAuth types compiled. No live install. No token in the repo.";

#[cfg(not(feature = "slack-oauth"))]
const CAPABILITY_DETAIL: &str = "Official Slack OAuth / workspace app (slack-morphism). Supported goal. Not a personal desktop clone. Feature slack-oauth is off. No token in the repo.";

const CAPABILITIES: ProtocolCapabilities = ProtocolCapabilities {
    id: ProtocolId::Slack,
    support: SupportClass::Supported,
    short_label: "Supported · OAuth-only",
    detail: CAPABILITY_DETAIL,
    official_api: true,
    allows_user_account_automation: false,
};

/// Official Slack path. Workspace install UI is out of scope for this spike.
#[derive(Debug)]
pub struct SlackAdapter;

impl SlackAdapter {
    #[must_use]
    pub const fn capabilities() -> ProtocolCapabilities {
        CAPABILITIES
    }

    /// True when `slack-oauth` compiled the `slack-morphism` types.
    #[must_use]
    pub const fn slack_oauth_compiled() -> bool {
        cfg!(feature = "slack-oauth")
    }

    /// Authorize URL for a workspace install. Does not include the client secret
    /// and does not contact Slack.
    pub fn install_url(
        vault: &dyn SlackSecretVault,
        source: &SlackApiSource,
        state: &str,
    ) -> Result<String, AdapterError> {
        let (client_id, secret) = require_slack_client(vault, source)?;
        drop(secret);
        Ok(authorize_url(&client_id, state))
    }

    fn seed_placeholders(&self, events: &EventTx) {
        emit_status(
            events,
            ProtocolId::Slack,
            AdapterStatus::Stubbed,
            CAPABILITIES.detail,
        );
        emit_conversation(
            events,
            Conversation {
                protocol: ProtocolId::Slack,
                id: "slack:example-channel".into(),
                title: "#general".into(),
                participant: "workspace".into(),
                preview: "OAuth workspace stub — not connected.".into(),
                unread: 1,
                order: 0,
            },
        );
        emit_message(
            events,
            ChatMessage {
                protocol: ProtocolId::Slack,
                conversation_id: "slack:example-channel".into(),
                id: "slack:example-channel:1".into(),
                sender: "thinwire".into(),
                body: "Slack is a supported OAuth / workspace-app goal. This pane is placeholder data; no workspace token is stored.".into(),
                outbound: false,
                delivery: Delivery::Sent,
            },
        );
    }
}

impl ProtocolAdapter for SlackAdapter {
    fn id(&self) -> ProtocolId {
        ProtocolId::Slack
    }

    fn capabilities(&self) -> ProtocolCapabilities {
        CAPABILITIES
    }

    fn start(&mut self, events: EventTx) {
        tracing::info!(
            compiled = Self::slack_oauth_compiled(),
            "slack adapter start (workspace oauth stub)"
        );
        self.seed_placeholders(&events);
    }

    fn handle(&mut self, command: AdapterCommand, events: &EventTx) -> Result<(), AdapterError> {
        match command {
            AdapterCommand::Connect {
                protocol: ProtocolId::Slack,
            }
            | AdapterCommand::Disconnect {
                protocol: ProtocolId::Slack,
            } => {
                emit_status(
                    events,
                    ProtocolId::Slack,
                    AdapterStatus::Stubbed,
                    CAPABILITIES.detail,
                );
                Ok(())
            }
            _ => Err(AdapterError::Unavailable {
                protocol: ProtocolId::Slack,
                reason: "command is not handled by the Slack stub",
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_flag_matches_this_build_and_stays_a_workspace_app() {
        assert_eq!(
            SlackAdapter::slack_oauth_compiled(),
            cfg!(feature = "slack-oauth")
        );
        let detail = SlackAdapter::capabilities().detail;
        assert!(detail.contains("workspace app"));
        assert!(detail.contains("slack-morphism"));
        assert!(detail.contains("Not a personal desktop clone"));
        assert!(!detail.to_ascii_lowercase().contains("reliable"));
        assert!(!SlackAdapter::capabilities().allows_user_account_automation);
    }

    #[test]
    fn install_url_omits_client_secret() {
        let vault = MemorySlackVault::new();
        vault.set_secret(SlackSecretKey::ClientId, "client-id-test");
        vault.set_secret(SlackSecretKey::ClientSecret, "client-secret-test");
        let url =
            SlackAdapter::install_url(&vault, &SlackApiSource::empty(), "state-test").expect("url");
        assert!(url.contains("client_id=client-id-test"));
        assert!(!url.contains("client-secret-test"));
        let missing = SlackAdapter::install_url(
            &MemorySlackVault::new(),
            &SlackApiSource::empty(),
            "state-test",
        );
        assert!(missing.is_err());
        let shown = missing.expect_err("missing").to_string();
        assert!(shown.contains("SLACK_CLIENT_ID"));
        assert!(!shown.contains("client-secret"));
    }

    #[test]
    fn default_ci_does_not_enable_slack_oauth() {
        let sources = [
            include_str!("../../Cargo.toml"),
            include_str!("../../../thinwire/Cargo.toml"),
            include_str!("../../../../.github/workflows/ci.yml"),
            include_str!("../../../../.github/workflows/os-zips.yml"),
            include_str!("../../../../scripts/test.sh"),
            include_str!("../../../../scripts/lint.sh"),
        ];
        for src in sources {
            assert!(
                !src.contains("--features slack-oauth"),
                "default scripts must not enable slack-oauth"
            );
            assert!(!src.contains("SLACK_CLIENT_ID"));
            assert!(!src.contains("SLACK_CLIENT_SECRET"));
            assert!(!src.contains("SLACK_APP_TOKEN"));
        }
        let protocol = include_str!("../../Cargo.toml");
        let feature = protocol
            .lines()
            .find(|line| line.contains("slack-oauth ="))
            .expect("feature");
        assert!(!feature.contains("hyper"));
        assert!(protocol.contains("default = []"));
    }

    #[test]
    fn commands_and_ui_do_not_carry_slack_secrets() {
        let adapter = include_str!("../adapter.rs");
        assert!(!adapter.contains("client_secret"));
        assert!(!adapter.contains("bot_token"));
        assert!(!adapter.contains("SlackAuth"));
        let auth = include_str!("../../../thinwire/src/app/auth.rs");
        let ui = include_str!("../../../thinwire/src/app/ui.rs");
        assert!(!auth.contains("Slack"));
        assert!(!auth.contains("SLACK_"));
        assert!(!ui.contains("SLACK_CLIENT"));
        assert!(!ui.contains("oauth/v2"));
        assert!(ui.contains("not ready"));
    }
}
