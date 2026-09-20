//! Critic risk copy. Keep these strings verbatim wherever the gate appears.

use super::ProtocolId;

/// Critic bullet 1. Do not soften.
pub const CRITIC_BULLET_1: &str = "WhatsApp (unofficial Web/linked-device) and Discord (user-account / self-bot) can get the user’s personal account banned or terminated. License-clean crates do not grant Meta or Discord permission.";

/// Critic bullet 2. Do not soften.
pub const CRITIC_BULLET_2: &str = "Do not call WhatsApp or Discord “reliable.” Unofficial WhatsApp clients and Discord user-account / self-bot paths can break or violate ToS. Signal is out of v1; this MIT binary does not link libsignal or Presage.";

/// Critic bullet 3. Do not soften.
pub const CRITIC_BULLET_3: &str = "“Fast and reliable” applies only to Telegram via official TDLib and Slack via official OAuth (workspace app, not a personal desktop clone). WhatsApp is experimental and Discord is bot/OAuth only. The app must not market them as production messaging.";

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
        assert!(
            !CRITIC_BULLET_2.contains("shipped") && CRITIC_BULLET_2.contains("Signal is out of v1"),
            "bullet 2 must not describe Signal as a shipped v1 module"
        );
        assert!(
            !CRITIC_BULLET_3.contains("other three"),
            "bullet 3 must not count Signal as a v1 experimental module"
        );
    }
}
