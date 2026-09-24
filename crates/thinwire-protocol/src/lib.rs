//! Protocol adapters, capability metadata, and the tokio host.

mod adapter;
mod discord;
mod fake;
mod host;
mod risk;
mod secrets;
mod slack;
mod telegram;
mod whatsapp;

pub use adapter::{
    AdapterCommand, AdapterError, AdapterEvent, AdapterStatus, ChatMessage, Conversation, Delivery,
    DiscordAuthMode, EventTx, ProtocolAdapter, ProtocolCapabilities, ProtocolId,
    RedactedPairingSecret, SupportClass, TelegramAuthError, TelegramAuthPhase, TelegramAuthStep,
    TelegramCodeVia,
};
pub use discord::{
    DISCORD_SECRET_BOT_TOKEN, DISCORD_SECRET_SERVICE, DiscordAdapter, DiscordOAuthInstall,
    DiscordSecretVault, MemoryDiscordVault,
};
pub use fake::FakeAdapter;
pub use host::AdapterHost;
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
pub use slack::{
    MemorySlackVault, SLACK_OAUTH_CALLBACK_PATH, SLACK_OAUTH_LOOPBACK_PORT, SLACK_SECRET_SERVICE,
    SlackAdapter, SlackApiOrigin, SlackApiSource, SlackCallbackError, SlackInstalledWorkspace,
    SlackSecretKey, SlackSecretVault, WORKSPACE_BOT_SCOPES, authorize_url, loopback_redirect_uri,
    new_oauth_state, parse_loopback_callback, resolve_slack_app_token, resolve_slack_client,
};
#[cfg(feature = "slack-oauth")]
pub use slack::{oauth_v2_access_request, socket_mode_config, workspace_bot_token};
pub use telegram::{
    TelegramAdapter, TelegramApiOrigin, TelegramApiSource, parse_telegram_chat_id,
    resolve_telegram_api, telegram_api_available,
};
pub use whatsapp::{WhatsAppAdapter, WhatsAppPhoneVault};

/// v1 protocols in shell display order (S2: four protocols, no Signal).
pub fn catalog() -> [ProtocolCapabilities; 4] {
    [
        telegram::TelegramAdapter::capabilities(),
        whatsapp::WhatsAppAdapter::capabilities(),
        discord::DiscordAdapter::capabilities(),
        slack::SlackAdapter::capabilities(),
    ]
}

pub(crate) fn registry(
    secrets: std::sync::Arc<dyn TelegramSecretVault>,
    discord: std::sync::Arc<dyn DiscordSecretVault>,
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
        Box::new(SlackAdapter),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_lists_v1_four_protocols_with_honest_support() {
        let caps = catalog();
        let ids: Vec<ProtocolId> = caps.iter().map(|c| c.id).collect();
        assert_eq!(
            ids,
            vec![
                ProtocolId::Telegram,
                ProtocolId::WhatsApp,
                ProtocolId::Discord,
                ProtocolId::Slack,
            ]
        );
        assert_eq!(ProtocolId::ALL.as_slice(), ids.as_slice());
        assert_eq!(caps.len(), 4);

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
        assert!(
            !ids.iter()
                .any(|id| id.display_name().eq_ignore_ascii_case("signal"))
        );
    }

    #[test]
    fn experimental_labels_avoid_banned_marketing_words() {
        for caps in catalog() {
            let short = caps.short_label;
            let detail = caps.detail;
            let blob = format!("{short} {detail}").to_ascii_lowercase();
            if matches!(caps.id, ProtocolId::WhatsApp | ProtocolId::Discord) {
                let name = caps.id.display_name();
                assert!(!contains_word(&blob, "reliable"), "{name}");
                assert!(!contains_word(&blob, "production"), "{name}");
                assert!(!contains_word(&blob, "official"), "{name}");
            }
        }
    }

    #[test]
    fn v1_does_not_depend_on_agpl_signal_client_or_presage() {
        // Signal the messenger stays out of v1. Reject Presage and the AGPL
        // Signal client crates. `wacore-libsignal` is MIT code inside the
        // optional whatsapp-rust linked-device stack, not a Signal account.
        for manifest in [
            include_str!("../Cargo.toml"),
            include_str!("../../thinwire/Cargo.toml"),
            include_str!("../../../Cargo.toml"),
        ] {
            let lower = manifest.to_ascii_lowercase();
            assert!(
                !lower.contains("presage"),
                "manifests must not name presage"
            );
            assert!(
                !lower.contains("libsignal"),
                "manifests must not name the Signal client crate"
            );
        }
        let names = package_names_from_lock(include_str!("../../../Cargo.lock"));
        for name in &names {
            assert!(
                !is_forbidden_signal_stack(name),
                "v1 must not depend on {name}"
            );
        }
        if names.contains(&"wacore-libsignal") {
            assert!(
                names.contains(&"whatsapp-rust"),
                "bundled linked-device crypto is only allowed via whatsapp-rust"
            );
        }
        assert!(is_forbidden_signal_stack("libsignal"));
        assert!(is_forbidden_signal_stack("libsignal-protocol"));
        assert!(is_forbidden_signal_stack("presage"));
        assert!(is_forbidden_signal_stack("presage-store-sled"));
        assert!(!is_forbidden_signal_stack("wacore-libsignal"));
        assert!(!is_forbidden_signal_stack("whatsapp-rust"));
    }

    fn package_names_from_lock(lock: &str) -> Vec<&str> {
        lock.lines()
            .filter_map(|line| {
                let rest = line.strip_prefix("name = \"")?;
                rest.strip_suffix('"')
            })
            .collect()
    }

    fn is_forbidden_signal_stack(name: &str) -> bool {
        let lower = name.to_ascii_lowercase();
        lower == "presage"
            || lower.starts_with("presage-")
            || lower == "libsignal"
            || lower.starts_with("libsignal-")
    }

    fn contains_word(hay: &str, word: &str) -> bool {
        hay.split(|ch: char| !ch.is_ascii_alphabetic())
            .any(|token| token == word)
    }
}
