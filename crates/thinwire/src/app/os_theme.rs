//! The light or dark choice of the Linux desktop (ADR 0005).
//!
//! winit gives no system theme on Linux: on X11 it has none, and on Wayland
//! it gives only the theme of its own window frame. Then egui uses its
//! fallback theme, dark, also on a light desktop. This watch reads the
//! settings portal key `org.freedesktop.appearance` / `color-scheme`
//! (1 is dark, 2 is light, 0 is no choice) and listens for its
//! `SettingChanged` signal. [`OsTheme::fill`] puts the value into the raw
//! input of each frame, so the System mode of egui follows the desktop.
//! Other platforms keep the value of winit.

use eframe::egui;

/// Value of the portal key `color-scheme`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
enum ColorScheme {
    NoChoice,
    Dark,
    Light,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl ColorScheme {
    fn from_portal(value: Option<u32>) -> Self {
        match value {
            Some(1) => Self::Dark,
            Some(2) => Self::Light,
            _ => Self::NoChoice,
        }
    }

    const fn code(self) -> u8 {
        match self {
            Self::NoChoice => 0,
            Self::Dark => 1,
            Self::Light => 2,
        }
    }

    const fn from_code(code: u8) -> Self {
        match code {
            1 => Self::Dark,
            2 => Self::Light,
            _ => Self::NoChoice,
        }
    }

    const fn theme(self) -> Option<egui::Theme> {
        match self {
            Self::Dark => Some(egui::Theme::Dark),
            Self::Light => Some(egui::Theme::Light),
            Self::NoChoice => None,
        }
    }
}

/// Put the desktop choice into the raw input. With no choice, the value of
/// winit stays.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn fill_raw(raw_input: &mut egui::RawInput, scheme: ColorScheme) {
    if let Some(theme) = scheme.theme() {
        raw_input.system_theme = Some(theme);
    }
}

/// The latest desktop answer. `fill` is cheap to call on every frame.
pub(crate) struct OsTheme {
    #[cfg(target_os = "linux")]
    scheme: std::sync::Arc<std::sync::atomic::AtomicU8>,
}

#[cfg(target_os = "linux")]
impl OsTheme {
    /// Read the portal and listen for changes, on a thread of its own.
    pub(crate) fn start(ctx: egui::Context) -> Self {
        linux::start(ctx)
    }

    /// Give egui the desktop choice for this frame.
    pub(crate) fn fill(&self, raw_input: &mut egui::RawInput) {
        let code = self.scheme.load(std::sync::atomic::Ordering::Acquire);
        fill_raw(raw_input, ColorScheme::from_code(code));
    }
}

#[cfg(not(target_os = "linux"))]
impl OsTheme {
    /// winit reports the theme on this platform.
    pub(crate) fn start(_ctx: egui::Context) -> Self {
        Self {}
    }

    /// winit reports the theme on this platform.
    #[allow(clippy::unused_self)]
    pub(crate) fn fill(&self, _raw_input: &mut egui::RawInput) {}
}

#[cfg(target_os = "linux")]
mod linux {
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::sync::mpsc::SyncSender;
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    use eframe::egui;
    use zbus::blocking::Connection;
    use zbus::zvariant::OwnedValue;

    use super::ColorScheme;
    use crate::app::reduce_motion::linux::{portal, portal_u32, read_setting};

    const NAMESPACE: &str = "org.freedesktop.appearance";
    const KEY: &str = "color-scheme";
    /// How long startup waits for the first read, so the first frame has
    /// the right colors. The watch keeps going after this.
    const FIRST_READ: Duration = Duration::from_millis(200);

    pub(super) fn start(ctx: egui::Context) -> super::OsTheme {
        let scheme = Arc::new(AtomicU8::new(ColorScheme::NoChoice.code()));
        let value = Arc::clone(&scheme);
        let (tx, rx) = mpsc::sync_channel(1);
        let spawned = std::thread::Builder::new()
            .name("thinwire-theme".into())
            .spawn(move || watch(&value, &ctx, &tx));
        if let Err(error) = spawned {
            tracing::warn!(%error, "OS theme watch did not start");
        }
        let _ = rx.recv_timeout(FIRST_READ);
        super::OsTheme { scheme }
    }

    fn watch(value: &AtomicU8, ctx: &egui::Context, ready: &SyncSender<()>) {
        let Ok(connection) = Connection::session() else {
            tracing::debug!("session bus is not available for the theme");
            let _ = ready.send(());
            return;
        };
        let Ok(proxy) = portal(&connection) else {
            let _ = ready.send(());
            return;
        };
        let read = |proxy: &zbus::blocking::Proxy<'_>| {
            ColorScheme::from_portal(
                read_setting(proxy, NAMESPACE, KEY).and_then(|value| portal_u32(&value)),
            )
        };
        publish(value, ctx, read(&proxy));
        let _ = ready.send(());
        let reader = proxy.clone();
        let Ok(signals) = proxy.receive_signal("SettingChanged") else {
            tracing::debug!("the settings portal did not offer change signals");
            return;
        };
        for message in signals {
            let Ok((namespace, key, _)) =
                message.body().deserialize::<(String, String, OwnedValue)>()
            else {
                continue;
            };
            if namespace == NAMESPACE && key == KEY {
                publish(value, ctx, read(&reader));
            }
        }
    }

    fn publish(value: &AtomicU8, ctx: &egui::Context, scheme: ColorScheme) {
        if value.swap(scheme.code(), Ordering::Release) != scheme.code() {
            ctx.request_repaint();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_portal_values_map_to_a_theme() {
        assert_eq!(
            ColorScheme::from_portal(Some(1)).theme(),
            Some(egui::Theme::Dark)
        );
        assert_eq!(
            ColorScheme::from_portal(Some(2)).theme(),
            Some(egui::Theme::Light)
        );
        assert_eq!(ColorScheme::from_portal(Some(0)).theme(), None);
        assert_eq!(ColorScheme::from_portal(Some(7)).theme(), None);
        assert_eq!(ColorScheme::from_portal(None).theme(), None);
        for scheme in [ColorScheme::NoChoice, ColorScheme::Dark, ColorScheme::Light] {
            assert_eq!(ColorScheme::from_code(scheme.code()), scheme);
        }
    }

    /// A light desktop makes the System mode light. egui otherwise falls
    /// back to dark when winit gives no theme.
    #[test]
    fn a_light_desktop_makes_the_system_mode_light() {
        let ctx = egui::Context::default();
        ctx.set_theme(egui::ThemePreference::System);
        let mut raw = egui::RawInput::default();
        fill_raw(&mut raw, ColorScheme::Light);
        ctx.run_ui(raw, |_| {}).textures_delta.clear();
        assert_eq!(ctx.theme(), egui::Theme::Light);

        let mut raw = egui::RawInput::default();
        fill_raw(&mut raw, ColorScheme::Dark);
        ctx.run_ui(raw, |_| {}).textures_delta.clear();
        assert_eq!(ctx.theme(), egui::Theme::Dark);
    }

    /// With no choice on the desktop, the value of winit stays.
    #[test]
    fn no_desktop_choice_keeps_the_value_of_winit() {
        let mut raw = egui::RawInput {
            system_theme: Some(egui::Theme::Light),
            ..Default::default()
        };
        fill_raw(&mut raw, ColorScheme::NoChoice);
        assert_eq!(raw.system_theme, Some(egui::Theme::Light));
    }
}
