//! Automatic download of photos and videos: the setting, the cost of the
//! network, and the rule that joins them.
//!
//! A photo or a video downloads by itself only when [`allows`] says yes. The
//! open chat still downloads its photos as before, with every setting.

/// When photos and videos download with no click (Settings > Chats).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MediaDownload {
    /// Only when the user opens a chat or clicks Play, as before.
    Never,
    /// On a network that the OS does not mark as metered.
    #[default]
    Unmetered,
    /// On every network.
    Always,
}

impl MediaDownload {
    /// Every value in picker order.
    pub const ALL: [Self; 3] = [Self::Never, Self::Unmetered, Self::Always];

    /// Value written to `settings.toml`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::Unmetered => "unmetered",
            Self::Always => "always",
        }
    }

    /// Parse a `settings.toml` value. Quotes, spaces, and case are ignored.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().trim_matches('"').to_ascii_lowercase().as_str() {
            "never" => Some(Self::Never),
            "unmetered" => Some(Self::Unmetered),
            "always" => Some(Self::Always),
            _ => None,
        }
    }

    /// The text of the choice in Settings.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Never => "Never",
            Self::Unmetered => "On unmetered networks",
            Self::Always => "Always",
        }
    }
}

/// What the OS says about the cost of the network.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NetworkCost {
    /// The OS cannot tell, or the app did not ask yet.
    #[default]
    Unknown,
    /// No data limit: a normal Wi-Fi or a cable.
    Unmetered,
    /// A data limit: a phone network or a hotspot.
    Metered,
}

/// Photos and videos download by themselves now. An unknown network counts
/// as metered.
#[must_use]
pub const fn allows(setting: MediaDownload, cost: NetworkCost) -> bool {
    match setting {
        MediaDownload::Never => false,
        MediaDownload::Always => true,
        MediaDownload::Unmetered => matches!(cost, NetworkCost::Unmetered),
    }
}

/// The longest video that downloads by itself, in seconds. A clip with no
/// length (0: the protocol does not say) downloads too. Each adapter keeps
/// its limit of 100 MiB for each file.
pub const AUTO_VIDEO_MAX_SECS: u32 = 300;

/// The widest photo size that downloads by itself, in pixels. A bubble shows
/// no larger picture.
pub const AUTO_PHOTO_MAX_WIDTH: u32 = 1280;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_network_counts_as_metered() {
        assert!(!allows(MediaDownload::Unmetered, NetworkCost::Unknown));
        assert!(!allows(MediaDownload::Unmetered, NetworkCost::Metered));
        assert!(allows(MediaDownload::Unmetered, NetworkCost::Unmetered));
        assert!(allows(MediaDownload::Always, NetworkCost::Metered));
        assert!(!allows(MediaDownload::Never, NetworkCost::Unmetered));
    }

    #[test]
    fn a_value_survives_the_file() {
        for value in MediaDownload::ALL {
            assert_eq!(MediaDownload::parse(value.as_str()), Some(value));
        }
        assert_eq!(
            MediaDownload::parse(" \"Always\" "),
            Some(MediaDownload::Always)
        );
        assert_eq!(MediaDownload::parse("sometimes"), None);
    }
}
