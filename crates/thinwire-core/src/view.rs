//! Read-only view of the core state for one frame or one redraw.

use std::ops::Deref;

use crate::ThemeMode;
use crate::clock::{Clock, ViewNow};
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
