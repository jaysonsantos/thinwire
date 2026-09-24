//! Frontend-independent thinwire core (ADR 0010).
//!
//! A frontend (egui, a TUI, a headless test) sends [`Intent`] values and
//! redraws when the [`ChangeSignal`] fires. The core owns the state, the
//! secret store, the settings, and the adapter host. Protocol work stays on
//! the tokio worker. This crate never depends on egui, eframe, or winit.

mod intent;
mod secret_text;
pub mod secrets;
pub mod settings;
mod signal;
mod theme;

pub use intent::{AuthField, DiscordIntent, Intent, SlackIntent, TelegramIntent, WhatsAppIntent};
pub use secret_text::SecretText;
pub use signal::{ChangeNotifier, ChangeSignal, change_channel};
pub use theme::ThemeMode;
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
}
