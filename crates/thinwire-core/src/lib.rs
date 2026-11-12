//! Frontend-independent thinwire core (ADR 0010).
//!
//! A frontend (egui, a TUI, a headless test) sends [`Intent`] values and
//! redraws when the [`ChangeSignal`] fires. The core owns the state, the
//! secret store, the settings, and the adapter host. Protocol work stays on
//! the tokio worker. This crate never depends on egui, eframe, winit, or an
//! audio playback crate.

// Local macOS builds only. See `security_cli.rs`, `build.rs` and
// `scripts/check-local-only-features.sh`: it never reaches CI or a release.
#[cfg(all(feature = "keychain-security-cli", not(target_os = "macos")))]
compile_error!(
    "feature `keychain-security-cli` is for macOS local builds: it runs /usr/bin/security"
);

mod clock;
mod core;
pub mod demo;
pub mod instance;
mod intent;
pub mod mutes;
pub mod notify;
mod paging;
mod secret_text;
pub mod secrets;
#[cfg(all(feature = "keychain-security-cli", target_os = "macos"))]
mod security_cli;
mod sends;
pub mod settings;
mod show;
mod signal;
pub mod signal_notice;
pub mod state;
mod store_key;
mod theme;
mod typing;
mod view;

pub use crate::clock::{Clock, ViewNow};
pub use crate::core::{Core, CoreConfig, HelperBuilder, HelperPaths};
pub use intent::{
    AuthField, DiscordIntent, Intent, SignalIntent, SlackIntent, TelegramIntent, WhatsAppIntent,
};
pub use secret_text::SecretText;
pub use signal::{ChangeNotifier, ChangeSignal, change_channel};
pub use state::{AuthKey, ForwardPicker, Pane, SplitSide};
pub use store_key::{
    CreateStoreKeyError, LoadStoreKey, SIGNAL_STORE_KEY_ACCOUNT, StoreKey, StoreKeyAction,
    StoreKeyPolicy, WHATSAPP_STORE_KEY_ACCOUNT, create_store_key, load_store_key, store_key_action,
    store_key_policy,
};
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
        for name in ["egui", "eframe", "winit", "rodio", "cpal", "symphonia"] {
            assert!(
                !deps.lines().any(|line| line.trim_start().starts_with(name)),
                "thinwire-core must not depend on {name}"
            );
        }
    }

    /// The dependency check runs in public CI, so it must not turn on
    /// `telegram-tdlib` (TDLib download). It keeps the GUI and playback bans.
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
        for name in ["egui", "eframe", "winit", "rodio", "cpal", "symphonia"] {
            assert!(script.contains(name), "the check still rejects {name}");
        }
        assert!(
            script.contains("crates/thinwire/src/app/audio/engine.rs"),
            "playback imports stay in the desktop engine"
        );
    }
}
