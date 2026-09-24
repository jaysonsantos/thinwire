//! Protocol adapters, capability metadata, and the tokio host.

mod adapter;
mod discord;
mod fake;
mod host;
mod risk;
mod secrets;
mod signal;
mod slack;
mod telegram;
mod whatsapp;

pub use adapter::{
    AccountState, AdapterCommand, AdapterError, AdapterEvent, AdapterStatus, ChatMessage,
    Conversation, Delivery, DiscordAuthMode, EventTx, ProtocolAdapter, ProtocolCapabilities,
    ProtocolId, RedactedPairingSecret, SupportClass, TelegramAuthError, TelegramAuthPhase,
    TelegramAuthStep, TelegramCodeVia,
};
pub use discord::{
    DISCORD_SECRET_BOT_TOKEN, DISCORD_SECRET_SERVICE, DiscordAdapter, DiscordOAuthInstall,
    DiscordSecretVault, MemoryDiscordVault,
};
pub use fake::FakeAdapter;
pub use host::{AdapterHost, HostSender};
pub use risk::{
    CRITIC_BULLET_1, CRITIC_BULLET_2, CRITIC_BULLET_3, CRITIC_RISK_BULLETS, critic_bullets_for,
    requires_experimental_gate,
};
pub use secrets::{
    MemorySecretVault, TDLIB_FOLDER, TDLIB_KEYUTILS_FOLDER, TELEGRAM_SECRET_API_HASH,
    TELEGRAM_SECRET_API_ID, TELEGRAM_SECRET_CODE, TELEGRAM_SECRET_DB_KEY, TELEGRAM_SECRET_PASSWORD,
    TELEGRAM_SECRET_PHONE, TELEGRAM_SECRET_SERVICE, TELEGRAM_SECRET_SESSION, TelegramSecretKey,
    TelegramSecretVault,
};
pub use signal::SignalAdapter;
pub use slack::{
    MemorySlackVault, SLACK_CONVERSATION_PREFIX, SLACK_OAUTH_CALLBACK_PATH,
    SLACK_OAUTH_LOOPBACK_PORT, SLACK_SECRET_SERVICE, SlackAdapter, SlackApiError, SlackApiOrigin,
    SlackApiSource, SlackAppToken, SlackBotToken, SlackBrowser, SlackCallbackError, SlackChannel,
    SlackChannelKind, SlackChannelPage, SlackCodeExchange, SlackDeps, SlackEventSource,
    SlackEventStream, SlackInbound, SlackInbox, SlackInstallGrant, SlackInstalledWorkspace,
    SlackLoopback, SlackPost, SlackSecretKey, SlackSecretVault, SlackWebApi, WORKSPACE_BOT_SCOPES,
    authorize_url, loopback_redirect_uri, new_oauth_state, parse_loopback_callback,
    resolve_slack_app_token, resolve_slack_client,
};
#[cfg(feature = "slack-oauth")]
pub use slack::{oauth_v2_access_request, socket_mode_config, workspace_bot_token};
pub use telegram::{
    TelegramAdapter, TelegramApiOrigin, TelegramApiSource, parse_telegram_chat_id,
    resolve_telegram_api, telegram_api_available,
};
pub use whatsapp::{WhatsAppAdapter, WhatsAppPhoneVault};

/// Shell protocols in display order. Signal is local-only and hidden unless `signal-local` is on.
pub fn catalog() -> [ProtocolCapabilities; 5] {
    [
        telegram::TelegramAdapter::capabilities(),
        whatsapp::WhatsAppAdapter::capabilities(),
        discord::DiscordAdapter::capabilities(),
        slack::SlackAdapter::capabilities(),
        signal::SignalAdapter::capabilities(),
    ]
}

pub(crate) fn registry(
    secrets: std::sync::Arc<dyn TelegramSecretVault>,
    discord: std::sync::Arc<dyn DiscordSecretVault>,
    slack: std::sync::Arc<dyn SlackSecretVault>,
    whatsapp_phone: std::sync::Arc<WhatsAppPhoneVault>,
    login_epoch: adapter::LoginEpoch,
) -> Vec<Box<dyn ProtocolAdapter>> {
    vec![
        Box::new(TelegramAdapter::with_login_epoch(
            secrets,
            crate::telegram::TelegramApiSource::from_build(),
            login_epoch,
        )),
        Box::new(WhatsAppAdapter::new(whatsapp_phone)),
        Box::new(DiscordAdapter::new(discord)),
        slack::registry_adapter(slack),
        Box::new(signal::SignalAdapter::new()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_lists_protocols_with_honest_support() {
        let caps = catalog();
        let ids: Vec<ProtocolId> = caps.iter().map(|c| c.id).collect();
        assert_eq!(
            ids,
            vec![
                ProtocolId::Telegram,
                ProtocolId::WhatsApp,
                ProtocolId::Discord,
                ProtocolId::Slack,
                ProtocolId::Signal,
            ]
        );
        assert_eq!(ProtocolId::ALL.as_slice(), ids.as_slice());
        assert_eq!(caps.len(), 5);

        let by_id = |id| caps.iter().find(|c| c.id == id).expect("protocol");
        assert_eq!(by_id(ProtocolId::Telegram).support, SupportClass::Supported);
        assert!(by_id(ProtocolId::Telegram).official_api);
        assert_eq!(by_id(ProtocolId::Slack).support, SupportClass::Supported);
        assert!(by_id(ProtocolId::Slack).official_api);
        assert_eq!(
            by_id(ProtocolId::WhatsApp).support,
            SupportClass::Experimental
        );
        assert!(!by_id(ProtocolId::WhatsApp).official_api);
        assert_eq!(
            by_id(ProtocolId::Discord).support,
            SupportClass::Constrained
        );
        assert!(!by_id(ProtocolId::Discord).allows_user_account_automation);
        assert_eq!(
            by_id(ProtocolId::Signal).support,
            SupportClass::Experimental
        );
        assert!(!by_id(ProtocolId::Signal).official_api);
        assert!(
            by_id(ProtocolId::Signal).detail.contains("signal-local")
                || by_id(ProtocolId::Signal).detail.contains("presage")
        );
    }

    #[test]
    fn experimental_labels_avoid_banned_marketing_words() {
        for caps in catalog() {
            let short = caps.short_label;
            let detail = caps.detail;
            let blob = format!("{short} {detail}").to_ascii_lowercase();
            if matches!(
                caps.id,
                ProtocolId::WhatsApp | ProtocolId::Discord | ProtocolId::Signal
            ) {
                let name = caps.id.display_name();
                assert!(!contains_word(&blob, "reliable"), "{name}");
                assert!(!contains_word(&blob, "production"), "{name}");
                assert!(!contains_word(&blob, "official"), "{name}");
            }
        }
    }

    #[test]
    fn agpl_clients_stay_behind_optional_features() {
        let protocol = include_str!("../Cargo.toml");
        let app = include_str!("../../thinwire/Cargo.toml");
        let core = include_str!("../../thinwire-core/Cargo.toml");
        let workspace = include_str!("../../../Cargo.toml");
        for manifest in [protocol, app, core, workspace] {
            assert!(
                !manifest_default_enables(manifest, "signal-local"),
                "signal-local must stay off the default feature set"
            );
            assert!(
                !manifest_default_enables(manifest, "whatsapp-web"),
                "whatsapp-web must stay off the default feature set"
            );
        }
        assert!(protocol.contains("signal-local"));
        assert!(protocol.contains("optional = true"));
        assert!(app.contains("signal-local"));
        assert!(core.contains("signal-local"));
        let release = include_str!("../../../.github/workflows/os-zips.yml");
        assert!(release.contains("--features telegram-tdlib"));
        assert!(!release.contains("signal-local"));
        assert!(!release.contains("whatsapp-web"));
    }

    fn manifest_default_enables(manifest: &str, feature: &str) -> bool {
        manifest.lines().any(|line| {
            let trimmed = line.trim();
            trimmed.starts_with("default") && trimmed.contains(feature) && !trimmed.contains('#')
        })
    }

    fn contains_word(hay: &str, word: &str) -> bool {
        hay.split(|ch: char| !ch.is_ascii_alphabetic())
            .any(|token| token == word)
    }
}
