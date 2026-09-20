//! Critic risk copy. Keep these strings verbatim wherever the gate appears.

use super::ProtocolId;

/// Critic bullet 1. Do not soften.
pub const CRITIC_BULLET_1: &str = "WhatsApp (unofficial Web/linked-device) and Discord (user-account / self-bot) can get the user’s personal account banned or terminated. License-clean crates do not grant Meta or Discord permission.";

/// Critic bullet 2. Do not soften.
pub const CRITIC_BULLET_2: &str = "Signal has no supported third-party client API. Breakage and unsigned clients are expected. Do not call Signal, WhatsApp, or Discord “reliable.”";

/// Critic bullet 3. Do not soften.
pub const CRITIC_BULLET_3: &str = "“Fast and reliable” applies only to Telegram via official TDLib and Slack via official OAuth (workspace app, not a personal desktop clone). The other three are experimental modules with ToS risk. The app must not market them as production messaging.";

/// All three Critic bullets, in order.
pub const CRITIC_RISK_BULLETS: [&str; 3] = [CRITIC_BULLET_1, CRITIC_BULLET_2, CRITIC_BULLET_3];

/// WhatsApp, Signal, and Discord must show a risk gate before any credential or QR step.
#[must_use]
pub const fn requires_experimental_gate(protocol: ProtocolId) -> bool {
    matches!(
        protocol,
        ProtocolId::WhatsApp | ProtocolId::Signal | ProtocolId::Discord
    )
}

/// Critic bullets that apply to one protocol’s experimental / constrained gate.
#[must_use]
pub const fn critic_bullets_for(protocol: ProtocolId) -> &'static [&'static str] {
    match protocol {
        ProtocolId::WhatsApp | ProtocolId::Discord => {
            &[CRITIC_BULLET_1, CRITIC_BULLET_2, CRITIC_BULLET_3]
        }
        ProtocolId::Signal => &[CRITIC_BULLET_2, CRITIC_BULLET_3],
        ProtocolId::Telegram | ProtocolId::Slack => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_covers_experimental_and_constrained_only() {
        assert!(requires_experimental_gate(ProtocolId::WhatsApp));
        assert!(requires_experimental_gate(ProtocolId::Signal));
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
    fn critic_bullets_are_verbatim_and_non_empty_for_gated_protocols() {
        for bullet in CRITIC_RISK_BULLETS {
            assert!(!bullet.is_empty());
        }
        assert!(critic_bullets_for(ProtocolId::WhatsApp).contains(&CRITIC_BULLET_1));
        assert!(critic_bullets_for(ProtocolId::Signal).contains(&CRITIC_BULLET_2));
        assert!(critic_bullets_for(ProtocolId::Discord).contains(&CRITIC_BULLET_1));
        assert!(critic_bullets_for(ProtocolId::Telegram).is_empty());
        assert_eq!(
            critic_bullets_for(ProtocolId::WhatsApp),
            CRITIC_RISK_BULLETS.as_slice()
        );
        assert_eq!(
            critic_bullets_for(ProtocolId::Signal),
            &[CRITIC_BULLET_2, CRITIC_BULLET_3]
        );
        assert_eq!(
            critic_bullets_for(ProtocolId::Discord),
            CRITIC_RISK_BULLETS.as_slice()
        );
    }
}
