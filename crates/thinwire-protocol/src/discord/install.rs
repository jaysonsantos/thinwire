//! Bot OAuth install shape for a guild inbox. Scope is `bot` only.

/// Guild inbox permission bits: `VIEW_CHANNEL` | `READ_MESSAGE_HISTORY`.
///
/// Send, manage, and administrator bits are absent. This is an inbox spike.
pub const INBOX_PERMISSION_BITS: u64 = (1 << 10) | (1 << 16);

/// Gateway intents for a guild inbox: `GUILDS` | `GUILD_MESSAGES` | `MESSAGE_CONTENT`.
pub const INBOX_INTENT_BITS: u64 = 1 | (1 << 9) | (1 << 15);

/// Direct-message intents. The inbox mask must not overlap these bits.
pub const PERSONAL_DM_INTENT_BITS: u64 = (1 << 12) | (1 << 13) | (1 << 14) | (1 << 25);

const _: () = assert!(INBOX_INTENT_BITS & PERSONAL_DM_INTENT_BITS == 0);

/// OAuth install for a Discord bot. `application_id` `0` means unset (no URL).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiscordOAuthInstall {
    application_id: u64,
}

impl DiscordOAuthInstall {
    /// OAuth scope. Guild bot install only.
    pub const SCOPE: &'static str = "bot";

    #[must_use]
    pub const fn inbox(application_id: u64) -> Self {
        Self { application_id }
    }

    #[must_use]
    pub const fn application_id(self) -> u64 {
        self.application_id
    }

    #[must_use]
    pub const fn scope(self) -> &'static str {
        Self::SCOPE
    }

    #[must_use]
    pub const fn permission_bits(self) -> u64 {
        INBOX_PERMISSION_BITS
    }

    /// Install URL. `None` when the application id is unset, so the tree has no client id.
    pub fn authorize_url(self) -> Option<String> {
        if self.application_id == 0 {
            return None;
        }
        Some(format!(
            "https://discord.com/oauth2/authorize?client_id={}&permissions={}&scope={}",
            self.application_id,
            self.permission_bits(),
            self.scope()
        ))
    }
}

#[cfg(feature = "discord-bot")]
#[must_use]
pub fn inbox_intents() -> twilight_model::gateway::Intents {
    twilight_model::gateway::Intents::from_bits_truncate(INBOX_INTENT_BITS)
}

#[cfg(feature = "discord-bot")]
#[must_use]
pub fn inbox_permissions() -> twilight_model::guild::Permissions {
    twilight_model::guild::Permissions::from_bits_truncate(INBOX_PERMISSION_BITS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_is_bot_scope_without_direct_messages() {
        assert_eq!(INBOX_INTENT_BITS & PERSONAL_DM_INTENT_BITS, 0);
        assert_eq!(DiscordOAuthInstall::SCOPE, "bot");
        let unset = DiscordOAuthInstall::inbox(0);
        assert!(unset.authorize_url().is_none());
        let url = DiscordOAuthInstall::inbox(12345)
            .authorize_url()
            .expect("url");
        assert!(url.contains("scope=bot"));
        assert!(url.contains(&format!("permissions={INBOX_PERMISSION_BITS}")));
        assert!(!url.contains("identify"));
        assert!(!url.contains("email"));
        assert!(!url.contains("guilds"));
        assert!(!url.to_ascii_lowercase().contains("token"));
    }

    #[cfg(feature = "discord-bot")]
    #[test]
    fn twilight_inbox_mask_matches_guild_intents_only() {
        use twilight_model::gateway::Intents;
        use twilight_model::guild::Permissions;

        let intents = inbox_intents();
        assert_eq!(intents.bits(), INBOX_INTENT_BITS);
        assert_eq!(intents & Intents::DIRECT_MESSAGES, Intents::empty());
        assert_eq!(
            intents & Intents::DIRECT_MESSAGE_REACTIONS,
            Intents::empty()
        );
        assert_eq!(intents & Intents::DIRECT_MESSAGE_TYPING, Intents::empty());
        assert!(intents.contains(Intents::GUILDS));
        assert!(intents.contains(Intents::GUILD_MESSAGES));
        assert!(intents.contains(Intents::MESSAGE_CONTENT));

        let permissions = inbox_permissions();
        assert_eq!(permissions.bits(), INBOX_PERMISSION_BITS);
        assert!(permissions.contains(Permissions::VIEW_CHANNEL));
        assert!(permissions.contains(Permissions::READ_MESSAGE_HISTORY));
        assert!(!permissions.contains(Permissions::SEND_MESSAGES));
        assert!(!permissions.contains(Permissions::ADMINISTRATOR));
    }
}
