//! Discord channel permission math for the bot member. Pure bit logic.
//!
//! Order follows the Discord permission docs: guild base, then the `@everyone`
//! overwrite, then role overwrites together, then the member overwrite.

use super::api::{Overwrite, OverwriteTarget};

pub(crate) const ADMINISTRATOR: u64 = 1 << 3;
pub(crate) const VIEW_CHANNEL: u64 = 1 << 10;
pub(crate) const SEND_MESSAGES: u64 = 1 << 11;
pub(crate) const READ_MESSAGE_HISTORY: u64 = 1 << 16;

/// Bits the inbox needs to list a channel and load its history.
pub(crate) const READ_BITS: u64 = VIEW_CHANNEL | READ_MESSAGE_HISTORY;

/// Inputs for one guild. `base` excludes channel overwrites.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MemberBase<'a> {
    pub guild_id: u64,
    pub member_id: u64,
    pub owner: bool,
    pub base: u64,
    pub roles: &'a [u64],
}

/// Effective channel permissions for the bot member.
#[must_use]
pub(crate) fn channel_permissions(member: MemberBase<'_>, overwrites: &[Overwrite]) -> u64 {
    if member.owner || member.base & ADMINISTRATOR != 0 {
        return u64::MAX;
    }
    let mut perms = member.base;

    if let Some(everyone) = overwrites
        .iter()
        .find(|o| o.target == OverwriteTarget::Role(member.guild_id))
    {
        perms = (perms & !everyone.deny) | everyone.allow;
    }

    let (mut allow, mut deny) = (0, 0);
    for overwrite in overwrites {
        if let OverwriteTarget::Role(role) = overwrite.target
            && role != member.guild_id
            && member.roles.contains(&role)
        {
            allow |= overwrite.allow;
            deny |= overwrite.deny;
        }
    }
    perms = (perms & !deny) | allow;

    if let Some(own) = overwrites
        .iter()
        .find(|o| o.target == OverwriteTarget::Member(member.member_id))
    {
        perms = (perms & !own.deny) | own.allow;
    }

    if perms & VIEW_CHANNEL == 0 {
        return 0;
    }
    perms
}

#[must_use]
pub(crate) const fn can_read(perms: u64) -> bool {
    perms & READ_BITS == READ_BITS
}

#[must_use]
pub(crate) const fn can_send(perms: u64) -> bool {
    can_read(perms) && perms & SEND_MESSAGES != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUILD: u64 = 100;
    const BOT: u64 = 7;
    const ROLE: u64 = 55;

    fn member(base: u64, roles: &[u64]) -> MemberBase<'_> {
        MemberBase {
            guild_id: GUILD,
            member_id: BOT,
            owner: false,
            base,
            roles,
        }
    }

    fn ow(target: OverwriteTarget, allow: u64, deny: u64) -> Overwrite {
        Overwrite {
            target,
            allow,
            deny,
        }
    }

    #[test]
    fn base_permissions_apply_without_overwrites() {
        let perms = channel_permissions(member(READ_BITS, &[]), &[]);
        assert!(can_read(perms));
        assert!(!can_send(perms));
        let perms = channel_permissions(member(READ_BITS | SEND_MESSAGES, &[]), &[]);
        assert!(can_send(perms));
    }

    #[test]
    fn everyone_deny_hides_the_channel_and_role_allow_restores_it() {
        let hidden = [ow(OverwriteTarget::Role(GUILD), 0, VIEW_CHANNEL)];
        assert_eq!(channel_permissions(member(READ_BITS, &[ROLE]), &hidden), 0);

        let restored = [
            ow(OverwriteTarget::Role(GUILD), 0, VIEW_CHANNEL),
            ow(OverwriteTarget::Role(ROLE), VIEW_CHANNEL, 0),
        ];
        assert!(can_read(channel_permissions(
            member(READ_BITS, &[ROLE]),
            &restored
        )));
        // A role overwrite for a role the bot does not have is ignored.
        assert_eq!(channel_permissions(member(READ_BITS, &[]), &restored), 0);
    }

    #[test]
    fn member_overwrite_wins_over_roles() {
        let overwrites = [
            ow(OverwriteTarget::Role(ROLE), SEND_MESSAGES, 0),
            ow(OverwriteTarget::Member(BOT), 0, SEND_MESSAGES),
        ];
        let perms = channel_permissions(member(READ_BITS, &[ROLE]), &overwrites);
        assert!(can_read(perms));
        assert!(!can_send(perms));
    }

    #[test]
    fn administrator_and_owner_see_everything() {
        let deny_all = [ow(OverwriteTarget::Member(BOT), 0, u64::MAX)];
        assert!(can_send(channel_permissions(
            member(ADMINISTRATOR, &[]),
            &deny_all
        )));
        let mut owner = member(0, &[]);
        owner.owner = true;
        assert!(can_send(channel_permissions(owner, &deny_all)));
    }

    #[test]
    fn missing_history_permission_is_not_readable() {
        assert!(!can_read(channel_permissions(
            member(VIEW_CHANNEL | SEND_MESSAGES, &[]),
            &[]
        )));
    }
}
