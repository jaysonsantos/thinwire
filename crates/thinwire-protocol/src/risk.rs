//! Critic risk copy. Keep these strings verbatim wherever the gate appears.

use super::ProtocolId;

/// Critic bullet 1. Do not soften.
pub const CRITIC_BULLET_1: &str = "WhatsApp (unofficial Web/linked-device) and Discord (user-account / self-bot) can get the user’s personal account banned or terminated. License-clean crates do not grant Meta or Discord permission.";

/// Critic bullet 2. Do not soften.
pub const CRITIC_BULLET_2: &str = "Do not call WhatsApp or Discord “reliable.” Unofficial WhatsApp clients and Discord user-account / self-bot paths can break or violate ToS.";

/// Critic bullet 3. Do not soften.
pub const CRITIC_BULLET_3: &str = "“Fast and reliable” applies only to Telegram via official TDLib and Slack via official OAuth (workspace app, not a personal desktop clone). WhatsApp is experimental and Discord is bot/OAuth inbox only. The app must not market them as production messaging.";

/// All three Critic bullets, in order.
pub const CRITIC_RISK_BULLETS: [&str; 3] = [CRITIC_BULLET_1, CRITIC_BULLET_2, CRITIC_BULLET_3];

/// WhatsApp and Discord must show a risk gate before any credential or QR step.
#[must_use]
pub const fn requires_experimental_gate(protocol: ProtocolId) -> bool {
    matches!(protocol, ProtocolId::WhatsApp | ProtocolId::Discord)
}

/// Critic bullets that apply to one protocol’s experimental / constrained gate.
#[must_use]
pub const fn critic_bullets_for(protocol: ProtocolId) -> &'static [&'static str] {
    match protocol {
        ProtocolId::WhatsApp | ProtocolId::Discord => CRITIC_RISK_BULLETS.as_slice(),
        ProtocolId::Telegram | ProtocolId::Slack => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_covers_experimental_and_constrained_only() {
        assert!(requires_experimental_gate(ProtocolId::WhatsApp));
        assert!(requires_experimental_gate(ProtocolId::Discord));
        assert!(!requires_experimental_gate(ProtocolId::Telegram));
        assert!(!requires_experimental_gate(ProtocolId::Slack));
    }

    #[test]
    fn readme_keeps_critic_bullets_verbatim() {
        let readme = include_str!("../../../README.md");
        for bullet in CRITIC_RISK_BULLETS {
            assert!(
                readme.contains(bullet),
                "README must keep Critic bullet verbatim: {bullet}"
            );
        }
    }

    #[test]
    fn docs_record_whatsapp_spike_as_experimental_and_not_default_ui() {
        let agents = include_str!("../../../AGENTS.md");
        let roadmap = include_str!("../../../ROADMAP.md");
        let readme = include_str!("../../../README.md");
        for doc in [agents, roadmap, readme] {
            assert!(doc.contains("whatsapp-web"));
            assert!(
                doc.to_ascii_lowercase().contains("not the default ui"),
                "spike must stay off the default UI"
            );
        }
        assert!(agents.contains("Full-screen ToS/ban gate"));
        assert!(roadmap.contains("No ready WhatsApp account"));
    }

    #[test]
    fn readme_does_not_list_signal_as_v1_protocol() {
        let readme = include_str!("../../../README.md");
        assert!(
            !readme.contains("| Signal |"),
            "README protocol table must not list Signal as a v1 protocol"
        );
        assert!(
            readme.contains("Signal is out of v1"),
            "README must state that Signal is out of v1"
        );
        assert!(
            readme.contains("bot/OAuth inbox only"),
            "README must describe Discord v1 as bot/OAuth inbox only"
        );
    }

    #[test]
    fn docs_purge_five_protocol_language() {
        let docs = [
            include_str!("../../../README.md"),
            include_str!("../../../AGENTS.md"),
            include_str!("../../../decisions/0001-option-b-multi-protocol.md"),
            include_str!("../../../decisions/0004-signal-out-of-v1.md"),
        ];
        for doc in docs {
            let lower = doc.to_ascii_lowercase();
            assert!(
                !lower.contains("all-five"),
                "docs must not use all-five filename or phrasing"
            );
            assert!(
                !has_word(&lower, "five"),
                "docs must not use five-protocol language"
            );
        }
        let adr4 = include_str!("../../../decisions/0004-signal-out-of-v1.md");
        assert!(adr4.contains("0001-option-b-multi-protocol.md"));
        assert!(!adr4.contains("0001-option-b-all-five-protocols.md"));
    }

    fn has_word(hay: &str, word: &str) -> bool {
        hay.split(|ch: char| !ch.is_ascii_alphabetic())
            .any(|token| token == word)
    }

    #[test]
    fn critic_bullets_are_verbatim_and_non_empty_for_gated_protocols() {
        for bullet in CRITIC_RISK_BULLETS {
            assert!(!bullet.is_empty());
        }
        assert!(critic_bullets_for(ProtocolId::WhatsApp).contains(&CRITIC_BULLET_1));
        assert!(critic_bullets_for(ProtocolId::Discord).contains(&CRITIC_BULLET_1));
        assert!(critic_bullets_for(ProtocolId::Telegram).is_empty());
        assert_eq!(
            critic_bullets_for(ProtocolId::WhatsApp),
            CRITIC_RISK_BULLETS.as_slice()
        );
        assert_eq!(
            critic_bullets_for(ProtocolId::Discord),
            CRITIC_RISK_BULLETS.as_slice()
        );
        for bullet in CRITIC_RISK_BULLETS {
            assert!(
                !bullet.contains("Signal"),
                "shipped-risk bullets must not treat Signal as an in-app module: {bullet}"
            );
            assert!(
                !bullet.contains("five"),
                "shipped-risk bullets must not use five-protocol language: {bullet}"
            );
        }
    }
}
