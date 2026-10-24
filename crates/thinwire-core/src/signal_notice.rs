//! Copy of the full-screen Signal notice.
//!
//! [`SIGNAL_NOTICE_VERSION`] is a hash of that copy. The settings file stores
//! the hash when the user accepts the notice. A different hash means the copy
//! changed, so the notice shows again. There is no separate "don't show
//! again" control: accepting the notice is the opt-in.

/// Title of the notice.
pub const SIGNAL_NOTICE_HEADING: &str = "Signal experimental client";

/// The Signal risk line of the README (ADR 0013 decision 9).
pub const SIGNAL_RISK: &str = "Signal: no supported third-party API, expect breakage.";

/// What this client is.
pub const SIGNAL_NOTICE_BODY: &str = "thinwire links as a secondary device with presage and libsignal. Signal can change its service at any time, and this client can then stop.";

/// Where the client runs.
pub const SIGNAL_NOTICE_HELPER: &str = "The Signal client is AGPL-3.0 code. It runs in a separate program, thinwire-signal-helper, which starts after you accept this notice.";

/// The notice shows no provisioning URL.
pub const SIGNAL_NOTICE_NO_URL: &str = "No provisioning URL is shown on this screen.";

/// The accept button. Accepting is the opt-in.
pub const SIGNAL_NOTICE_ACCEPT: &str = "I understand — Signal can break at any time";

/// The notice copy, in screen order. The version hash covers these strings.
pub const SIGNAL_NOTICE_PARTS: [&str; 6] = [
    SIGNAL_NOTICE_HEADING,
    SIGNAL_RISK,
    SIGNAL_NOTICE_BODY,
    SIGNAL_NOTICE_HELPER,
    SIGNAL_NOTICE_NO_URL,
    SIGNAL_NOTICE_ACCEPT,
];

/// Identity of the notice copy. Saved acceptances of another value do not count.
pub const SIGNAL_NOTICE_VERSION: u64 = notice_version(&SIGNAL_NOTICE_PARTS);

const fn notice_version(parts: &[&str]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut i = 0;
    while i < parts.len() {
        let bytes = parts[i].as_bytes();
        let mut j = 0;
        while j < bytes.len() {
            hash ^= bytes[j] as u64;
            hash = hash.wrapping_mul(0x0100_0000_01b3);
            j += 1;
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x0100_0000_01b3);
        i += 1;
    }
    if hash == 0 { 1 } else { hash }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_notice_version_tracks_the_copy() {
        assert_ne!(SIGNAL_NOTICE_VERSION, 0);
        assert_eq!(SIGNAL_NOTICE_VERSION, notice_version(&SIGNAL_NOTICE_PARTS));
        let mut changed = SIGNAL_NOTICE_PARTS;
        changed[1] = "Signal: a different risk line.";
        assert_ne!(notice_version(&changed), SIGNAL_NOTICE_VERSION);
    }

    #[test]
    fn the_readme_keeps_the_signal_risk_line() {
        let readme = include_str!("../../../README.md");
        assert!(readme.contains(SIGNAL_RISK));
    }
}
