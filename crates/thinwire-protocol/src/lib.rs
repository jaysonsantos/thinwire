//! Protocol adapters, capability metadata, and the tokio host.

mod adapter;
mod discord;
mod fake;
mod host;
mod risk;
mod slack;
mod telegram;
mod whatsapp;

pub use adapter::{
    AdapterCommand, AdapterError, AdapterEvent, AdapterStatus, ChatMessage, Conversation,
    DiscordAuthMode, EventTx, ProtocolAdapter, ProtocolCapabilities, ProtocolId, SupportClass,
    TelegramAuthStep,
};
pub use discord::DiscordAdapter;
pub use fake::FakeAdapter;
pub use host::AdapterHost;
pub use risk::{
    CRITIC_BULLET_1, CRITIC_BULLET_2, CRITIC_BULLET_3, CRITIC_RISK_BULLETS, critic_bullets_for,
    requires_experimental_gate,
};
pub use slack::SlackAdapter;
pub use telegram::TelegramAdapter;
pub use telegram::{
    TELEGRAM_SECRET_API_HASH, TELEGRAM_SECRET_API_ID, TELEGRAM_SECRET_SERVICE,
    TELEGRAM_SECRET_SESSION,
};
pub use whatsapp::WhatsAppAdapter;

/// v1 protocols in shell display order (S2: four protocols, no Signal).
pub fn catalog() -> [ProtocolCapabilities; 4] {
    [
        telegram::TelegramAdapter::capabilities(),
        whatsapp::WhatsAppAdapter::capabilities(),
        discord::DiscordAdapter::capabilities(),
        slack::SlackAdapter::capabilities(),
    ]
}

pub(crate) fn registry() -> Vec<Box<dyn ProtocolAdapter>> {
    vec![
        Box::new(TelegramAdapter),
        Box::new(WhatsAppAdapter),
        Box::new(DiscordAdapter),
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
    fn v1_does_not_depend_on_libsignal_or_presage() {
        let sources = [
            include_str!("../Cargo.toml"),
            include_str!("../../thinwire/Cargo.toml"),
            include_str!("../../../Cargo.toml"),
            include_str!("../../../Cargo.lock"),
        ];
        for src in sources {
            let lower = src.to_ascii_lowercase();
            assert!(!lower.contains("presage"), "v1 must not depend on presage");
            assert!(
                !lower.contains("libsignal"),
                "v1 must not depend on libsignal"
            );
        }
    }

    fn contains_word(hay: &str, word: &str) -> bool {
        hay.split(|ch: char| !ch.is_ascii_alphabetic())
            .any(|token| token == word)
    }
}
