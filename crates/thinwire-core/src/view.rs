//! Read-only view of the core state for one frame or one redraw.

use std::ops::Deref;

use crate::ThemeMode;
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

    /// The stored light/dark preference.
    #[must_use]
    pub fn theme(&self) -> ThemeMode {
        self.settings.theme()
    }
}

impl Deref for View<'_> {
    type Target = Snapshot;

    fn deref(&self) -> &Snapshot {
        self.state
    }
}
