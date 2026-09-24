//! Frontend-independent thinwire core (ADR 0010).
//!
//! A frontend (egui, a TUI, a headless test) sends [`Intent`] values and
//! redraws when the [`ChangeSignal`] fires. The core owns the state, the
//! secret store, the settings, and the adapter host. Protocol work stays on
//! the tokio worker. This crate never depends on egui, eframe, or winit.

mod core;
mod intent;
mod secret_text;
pub mod secrets;
mod sends;
pub mod settings;
mod signal;
pub mod state;
mod theme;
mod view;

pub use crate::core::{Core, CoreConfig};
pub use intent::{
    AuthField, DiscordIntent, Intent, SignalIntent, SlackIntent, TelegramIntent, WhatsAppIntent,
};
pub use secret_text::SecretText;
pub use signal::{ChangeNotifier, ChangeSignal, change_channel};
pub use state::{AuthKey, InboxFilter};
pub use theme::ThemeMode;
pub use view::View;
// Frontends name protocols through the core, not through a second dependency.
pub use thinwire_protocol::ProtocolId;

#[cfg(test)]
mod tests {
    /// ADR 0010: the core manifest names no GUI toolkit crate.
    #[test]
    fn manifest_has_no_gui_toolkit() {
        let manifest = include_str!("../Cargo.toml");
        let deps = manifest
            .split_once("[dependencies]")
            .map_or("", |(_, rest)| rest);
        for name in ["egui", "eframe", "winit"] {
            assert!(
                !deps.lines().any(|line| line.trim_start().starts_with(name)),
                "thinwire-core must not depend on {name}"
            );
        }
    }

    /// The dependency check runs in public CI, so it must not turn on
    /// `telegram-tdlib` (TDLib download). It keeps the GUI assertion.
    #[test]
    fn dependency_check_stays_on_default_features() {
        let script = include_str!("../../../scripts/check-core-deps.sh");
        let command = script
            .lines()
            .find(|line| line.contains("cargo tree"))
            .expect("cargo tree call");
        assert!(!command.contains("--all-features"));
        assert!(!command.contains("--features"));
        assert!(!command.contains("telegram-tdlib"));
        for name in ["egui", "eframe", "winit"] {
            assert!(script.contains(name), "the check still rejects {name}");
        }
    }
}
