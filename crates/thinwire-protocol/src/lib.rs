//! Protocol adapters, capability metadata, and the tokio host.

mod adapter;
mod discord;
mod fake;
mod host;
mod risk;
mod signal;
mod slack;
mod telegram;
mod whatsapp;

pub use adapter::{
    AdapterCommand, AdapterError, AdapterEvent, AdapterStatus, ChatMessage, Conversation,
    DiscordAuthMode, EventTx, ProtocolAdapter, ProtocolCapabilities, ProtocolId, SupportClass,
};
pub use discord::DiscordAdapter;
pub use fake::FakeAdapter;
pub use host::AdapterHost;
pub use risk::{
    CRITIC_BULLET_1, CRITIC_BULLET_2, CRITIC_BULLET_3, CRITIC_RISK_BULLETS, critic_bullets_for,
    requires_experimental_gate,
};
pub use signal::SignalAdapter;
pub use slack::SlackAdapter;
pub use telegram::TelegramAdapter;
pub use whatsapp::WhatsAppAdapter;

/// All five product-lock protocols, in shell display order.
pub fn catalog() -> [ProtocolCapabilities; 5] {
    [
        telegram::TelegramAdapter::capabilities(),
        whatsapp::WhatsAppAdapter::capabilities(),
        signal::SignalAdapter::capabilities(),
        discord::DiscordAdapter::capabilities(),
        slack::SlackAdapter::capabilities(),
    ]
}

pub(crate) fn registry() -> Vec<Box<dyn ProtocolAdapter>> {
    vec![
        Box::new(TelegramAdapter),
        Box::new(WhatsAppAdapter),
        Box::new(SignalAdapter),
        Box::new(DiscordAdapter),
        Box::new(SlackAdapter),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_lists_all_five_protocols_with_honest_support() {
        let caps = catalog();
        let ids: Vec<ProtocolId> = caps.iter().map(|c| c.id).collect();
        assert_eq!(
            ids,
            vec![
                ProtocolId::Telegram,
                ProtocolId::WhatsApp,
                ProtocolId::Signal,
                ProtocolId::Discord,
                ProtocolId::Slack,
            ]
        );

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
            by_id(ProtocolId::Signal).support,
            SupportClass::Experimental
        );
        assert!(!by_id(ProtocolId::Signal).official_api);
        assert_eq!(
            by_id(ProtocolId::Discord).support,
            SupportClass::Constrained
        );
        assert!(!by_id(ProtocolId::Discord).allows_user_account_automation);
    }

    #[test]
    fn experimental_labels_avoid_banned_marketing_words() {
        for caps in catalog() {
            let short = caps.short_label;
            let detail = caps.detail;
            let blob = format!("{short} {detail}").to_ascii_lowercase();
            if matches!(
                caps.id,
                ProtocolId::WhatsApp | ProtocolId::Signal | ProtocolId::Discord
            ) {
                let name = caps.id.display_name();
                assert!(!contains_word(&blob, "reliable"), "{name}");
                assert!(!contains_word(&blob, "production"), "{name}");
                assert!(!contains_word(&blob, "official"), "{name}");
            }
        }
    }

    fn contains_word(hay: &str, word: &str) -> bool {
        hay.split(|ch: char| !ch.is_ascii_alphabetic())
            .any(|token| token == word)
    }
}
