//! Read-only view of the core state for one frame or one redraw.

use std::ops::Deref;

use thinwire_protocol::{ChatTopic, Conversation, ProtocolId, topic_mute_id};

use crate::ThemeMode;
use crate::clock::{Clock, ViewNow};
use crate::mutes::ChatMute;
use crate::secrets::{Persistence, SecretStore};
use crate::settings::Settings;
use crate::state::Snapshot;

/// Borrow of the state. It derefs to [`Snapshot`], so a frontend reads
/// fields and `&self` methods. It cannot call a mutating method: changes go
/// through [`crate::Core::dispatch`].
#[derive(Clone, Copy)]
pub struct View<'a> {
    pub(crate) state: &'a Snapshot,
    pub(crate) secrets: &'a SecretStore,
    pub(crate) settings: &'a Settings,
    pub(crate) clock: Clock,
}

impl View<'_> {
    /// Telegram API credentials exist: publisher inject or keychain override.
    #[must_use]
    pub fn has_api_credentials(&self) -> bool {
        self.state.has_api_credentials(self.secrets)
    }

    /// How long a sign-in lasts with the secret store in use.
    #[must_use]
    pub fn persistence(&self) -> Persistence {
        self.secrets.persistence()
    }

    /// Now, in the zone of the core's clock (#120). Format every time from
    /// this value, never from the system clock directly.
    #[must_use]
    pub fn now(&self) -> ViewNow {
        self.clock.now()
    }

    /// The stored light/dark preference.
    #[must_use]
    pub fn theme(&self) -> ThemeMode {
        self.settings.theme()
    }

    /// Settings switch: desktop notifications on (#32).
    #[must_use]
    pub fn notifications(&self) -> bool {
        self.settings.notifications()
    }

    /// Settings switch: notifications show the sender and the text (#32).
    #[must_use]
    pub fn notification_preview(&self) -> bool {
        self.settings.notification_preview()
    }

    /// The mute of one chat (#153). A protocol mute wins over a thinwire
    /// mute. A chat that is not listed has no protocol mute.
    #[must_use]
    pub fn chat_mute(&self, protocol: ProtocolId, conversation_id: &str) -> ChatMute {
        let protocol_muted = self
            .state
            .conversation(protocol, conversation_id)
            .is_some_and(|row| row.muted);
        let muted_here = self
            .settings
            .chat_mutes()
            .contains(protocol, conversation_id);
        ChatMute::of(protocol_muted, muted_here)
    }

    /// Unread messages of every chat that is not muted, in every protocol
    /// with a session. For the window title and a taskbar badge (#32).
    #[must_use]
    pub fn unread_total(&self) -> u32 {
        self.state.unread_total(self.settings.chat_mutes())
    }

    /// The badge of one inbox row. A forum sums unmuted topics.
    #[must_use]
    pub fn display_unread(&self, row: &Conversation) -> u32 {
        self.state.display_unread(row, self.settings.chat_mutes())
    }

    /// Unread on an account chip. A normal chat contributes its own count.
    /// A forum contributes the sum of unmuted topics.
    #[must_use]
    pub fn account_unread(&self, protocol: ProtocolId) -> u32 {
        self.state
            .account_display_unread(protocol, self.settings.chat_mutes())
    }

    /// Unread on one account chip, including a previous Signal number.
    #[must_use]
    pub fn account_row_unread(&self, account: &crate::state::AccountRow) -> u32 {
        self.state
            .account_display_unread_for(account, self.settings.chat_mutes())
    }

    /// The mute of one topic. A protocol mute wins.
    #[must_use]
    pub fn topic_mute(&self, topic: &ChatTopic) -> ChatMute {
        ChatMute::of(
            topic.muted,
            self.settings.chat_mutes().contains(
                topic.protocol,
                &topic_mute_id(&topic.conversation_id, topic.topic),
            ),
        )
    }

    /// The open topic, when the center pane shows one.
    #[must_use]
    pub fn open_topic(&self) -> Option<&ChatTopic> {
        self.selected_topic_row()
    }

    /// `Some` when the center pane shows the topic list of a forum or a bot.
    #[must_use]
    pub fn showing_topic_list(&self) -> bool {
        self.selected_conversation_row()
            .is_some_and(|row| row.has_topics && self.selected_topic.is_none())
    }

    /// Placeholder of the compose field.
    #[must_use]
    pub fn compose_hint(&self) -> String {
        match self.open_topic() {
            Some(topic) => format!("Message in {}", topic.name),
            None => "Message".into(),
        }
    }

    /// A closed topic does not take a message.
    #[must_use]
    pub fn topic_closed(&self) -> bool {
        self.open_topic().is_some_and(|topic| topic.closed)
    }
}

/// Test hook: a view over parts that a test owns.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
impl<'a> View<'a> {
    #[must_use]
    pub fn from_parts(
        state: &'a Snapshot,
        secrets: &'a SecretStore,
        settings: &'a Settings,
    ) -> Self {
        Self {
            state,
            secrets,
            settings,
            clock: Clock::System,
        }
    }

    /// The same view with another clock.
    #[must_use]
    pub const fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }
}

impl Deref for View<'_> {
    type Target = Snapshot;

    fn deref(&self) -> &Snapshot {
        self.state
    }
}
