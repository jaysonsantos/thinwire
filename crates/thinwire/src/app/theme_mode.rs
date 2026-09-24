//! egui side of the theme mode: preference mapping and live OS follow (ADR 0005).
//! The design tokens (colors, fonts, sizes) live in `theme.rs`.
//!
//! The core stores [`ThemeMode`] and writes `settings.toml`. This file only
//! maps the mode to egui and tracks the OS light/dark changes.

use eframe::egui;
use thinwire_core::ThemeMode;

/// egui mapping for the core [`ThemeMode`]. The core does not know egui.
pub trait ThemeModeEgui: Sized {
    fn to_egui(self) -> egui::ThemePreference;
    fn from_egui(preference: egui::ThemePreference) -> Self;
}

impl ThemeModeEgui for ThemeMode {
    fn to_egui(self) -> egui::ThemePreference {
        match self {
            Self::System => egui::ThemePreference::System,
            Self::Light => egui::ThemePreference::Light,
            Self::Dark => egui::ThemePreference::Dark,
        }
    }

    fn from_egui(preference: egui::ThemePreference) -> Self {
        match preference {
            egui::ThemePreference::System => Self::System,
            egui::ThemePreference::Light => Self::Light,
            egui::ThemePreference::Dark => Self::Dark,
        }
    }
}

/// Apply the stored mode. System follows the OS and updates when it changes.
pub fn apply(ctx: &egui::Context, mode: ThemeMode) {
    ctx.set_theme(mode.to_egui());
}

/// Keep System mode in sync with live OS changes (egui-winit `ThemeChanged`).
///
/// Returns true when the OS light/dark preference changed this frame so the
/// caller can request an immediate repaint.
pub fn follow_os_live(
    ctx: &egui::Context,
    mode: ThemeMode,
    last_os_theme: &mut Option<egui::Theme>,
) -> bool {
    apply(ctx, mode);
    live_os_theme_changed(mode, last_os_theme, ctx.system_theme())
}

/// True when preference is System and the polled OS theme changed.
#[must_use]
pub fn live_os_theme_changed(
    mode: ThemeMode,
    last: &mut Option<egui::Theme>,
    current: Option<egui::Theme>,
) -> bool {
    if mode != ThemeMode::System {
        return false;
    }
    let changed = last.zip(current).is_some_and(|(prev, now)| prev != now);
    if current.is_some() {
        *last = current;
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn egui_mapping_covers_all_modes() {
        for mode in ThemeMode::ALL {
            assert_eq!(ThemeMode::from_egui(mode.to_egui()), mode);
        }
    }

    #[test]
    fn system_mode_follows_live_os_theme_changes() {
        let mut last = None;
        assert!(!live_os_theme_changed(
            ThemeMode::System,
            &mut last,
            Some(egui::Theme::Dark)
        ));
        assert_eq!(last, Some(egui::Theme::Dark));
        assert!(live_os_theme_changed(
            ThemeMode::System,
            &mut last,
            Some(egui::Theme::Light)
        ));
        assert_eq!(last, Some(egui::Theme::Light));
        assert!(!live_os_theme_changed(
            ThemeMode::System,
            &mut last,
            Some(egui::Theme::Light)
        ));
    }

    #[test]
    fn light_or_dark_override_ignores_os_changes() {
        let mut last = Some(egui::Theme::Dark);
        assert!(!live_os_theme_changed(
            ThemeMode::Light,
            &mut last,
            Some(egui::Theme::Light)
        ));
        assert_eq!(last, Some(egui::Theme::Dark));
    }
}
