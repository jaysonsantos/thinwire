//! The OS reduce-motion setting (#247).
//!
//! macOS: Reduce Motion (`NSWorkspace`).
//! Windows: Show animations (`UISettings.AnimationsEnabled`).
//! Linux: the settings portal key `org.freedesktop.appearance` /
//! `reduced-motion`, the GNOME animation switch, and the KDE animation
//! factor. Any one of them that asks for less motion wins.
//!
//! macOS and Windows publish a new value when they send a change event.
//! The idle repaint reads it. Linux listens for the portal
//! `SettingChanged` signal and wakes the window. A missing answer leaves
//! motion on.

use std::time::Duration;

use eframe::egui;

/// How long startup waits for the first Linux read. The watch keeps going
/// after this and wakes the window when the value arrives.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const LINUX_FIRST_READ: Duration = Duration::from_millis(200);

/// `true` when these OS answers ask for less motion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
struct MotionSources {
    /// Portal `reduced-motion` is 1.
    portal_reduced: bool,
    /// GNOME `enable-animations` is false.
    animations_off: bool,
    /// KDE `AnimationDurationFactor` is 0.
    kde_instant: bool,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn sources_reduce(sources: MotionSources) -> bool {
    sources.portal_reduced || sources.animations_off || sources.kde_instant
}

/// The portal `SettingChanged` signals that can change reduce-motion.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn setting_event_is_motion(namespace: &str, key: &str) -> bool {
    matches!(
        (namespace, key),
        ("org.freedesktop.appearance", "reduced-motion")
            | ("org.gnome.desktop.interface", "enable-animations")
            | ("org.gnome.desktop.a11y.interface", "reduced-motion")
            | ("org.kde.kdeglobals.KDE", "AnimationDurationFactor")
    )
}

/// `true` when the KDE animation factor in `kdeglobals` is 0.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn kde_animation_factor_is_zero(text: &str) -> bool {
    let mut in_kde = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            in_kde = line == "[KDE]";
            continue;
        }
        if !in_kde || line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() == "AnimationDurationFactor" {
            return value.trim().parse::<f64>().ok() == Some(0.0);
        }
    }
    false
}

/// Latest OS answer. `reduced` is cheap to call on every frame.
pub(crate) struct OsReduceMotion {
    #[cfg(target_os = "linux")]
    reduced: std::sync::Arc<std::sync::atomic::AtomicBool>,
    #[cfg(windows)]
    settings: Option<windows_motion::Watch>,
}

#[cfg(target_os = "linux")]
impl OsReduceMotion {
    /// Read the OS setting and listen for portal `SettingChanged`.
    pub(crate) fn start(ctx: egui::Context) -> Self {
        linux::start(ctx)
    }

    /// `true` when the OS asks for less motion.
    pub(crate) fn reduced(&self) -> bool {
        self.reduced.load(std::sync::atomic::Ordering::Acquire)
    }
}

#[cfg(target_os = "macos")]
impl OsReduceMotion {
    /// The getter reads the live Reduce Motion value. A change event updates
    /// that value, and the idle repaint reads it again.
    pub(crate) fn start(_ctx: egui::Context) -> Self {
        Self {}
    }

    /// `true` when the OS asks for less motion.
    pub(crate) fn reduced(&self) -> bool {
        macos::reduced()
    }
}

#[cfg(windows)]
impl OsReduceMotion {
    /// Subscribe to the Show-animations change event.
    pub(crate) fn start(ctx: egui::Context) -> Self {
        Self {
            settings: windows_motion::Watch::start(ctx),
        }
    }

    /// `true` when the OS asks for less motion.
    pub(crate) fn reduced(&self) -> bool {
        self.settings
            .as_ref()
            .is_some_and(windows_motion::Watch::reduced)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
impl OsReduceMotion {
    pub(crate) fn start(_ctx: egui::Context) -> Self {
        Self {}
    }

    /// `true` when the OS asks for less motion.
    pub(crate) fn reduced(&self) -> bool {
        false
    }
}

#[cfg(target_os = "macos")]
mod macos {
    pub(super) fn reduced() -> bool {
        objc2_app_kit::NSWorkspace::sharedWorkspace().accessibilityDisplayShouldReduceMotion()
    }
}

#[cfg(windows)]
mod windows_motion {
    use eframe::egui;
    use windows::Foundation::TypedEventHandler;
    use windows::UI::ViewManagement::UISettings;

    pub(super) struct Watch {
        settings: UISettings,
    }

    impl Watch {
        /// Subscribe to the animation-setting event, and keep the settings
        /// object so a later read sees the new value.
        pub(super) fn start(ctx: egui::Context) -> Option<Self> {
            let settings = match UISettings::new() {
                Ok(settings) => settings,
                Err(error) => {
                    tracing::debug!(%error, "Windows animation setting is not available");
                    return None;
                }
            };
            let wake = ctx;
            if settings
                .AnimationsEnabledChanged(&TypedEventHandler::new(move |_, _| {
                    wake.request_repaint();
                    Ok(())
                }))
                .is_err()
            {
                tracing::debug!("Windows did not offer animation setting changes");
            }
            Some(Self { settings })
        }

        /// Show animations off means reduce motion.
        pub(super) fn reduced(&self) -> bool {
            self.settings.AnimationsEnabled().ok() == Some(false)
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::SyncSender;
    use std::sync::{Arc, mpsc};

    use eframe::egui;
    use zbus::blocking::{Connection, Proxy};
    use zbus::zvariant::{OwnedValue, Value};

    use super::{
        LINUX_FIRST_READ, MotionSources, kde_animation_factor_is_zero, setting_event_is_motion,
        sources_reduce,
    };

    const PORTAL_DEST: &str = "org.freedesktop.portal.Desktop";
    const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
    const PORTAL_IFACE: &str = "org.freedesktop.portal.Settings";
    /// Skip a huge or unexpected file.
    const KDEGLOBALS_LIMIT: u64 = 1024 * 1024;

    pub(super) fn start(ctx: egui::Context) -> super::OsReduceMotion {
        let reduced = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&reduced);
        let (tx, rx) = mpsc::sync_channel(1);
        let spawned = std::thread::Builder::new()
            .name("thinwire-motion".into())
            .spawn(move || watch(flag, ctx, tx));
        if let Err(error) = spawned {
            tracing::warn!(%error, "OS reduce-motion watch did not start");
        }
        let _ = rx.recv_timeout(LINUX_FIRST_READ);
        super::OsReduceMotion { reduced }
    }

    fn watch(flag: Arc<AtomicBool>, ctx: egui::Context, ready: SyncSender<()>) {
        let connection = match Connection::session() {
            Ok(connection) => connection,
            Err(error) => {
                tracing::debug!(%error, "session bus is not available");
                publish(&flag, &ctx, kde_file_is_instant());
                let _ = ready.send(());
                return;
            }
        };
        let Ok(proxy) = portal(&connection) else {
            publish(&flag, &ctx, kde_file_is_instant());
            let _ = ready.send(());
            return;
        };
        publish(&flag, &ctx, read_portal(&proxy));
        let _ = ready.send(());
        // The signal stream keeps its own proxy. Reads use this clone so a
        // change event can load the new value while the stream is open.
        let reader = proxy.clone();
        let Ok(signals) = proxy.receive_signal("SettingChanged") else {
            tracing::debug!("the settings portal did not offer change signals");
            return;
        };
        for message in signals {
            if motion_event(&message) {
                publish(&flag, &ctx, read_portal(&reader));
            }
        }
    }

    fn portal(connection: &Connection) -> zbus::Result<Proxy<'static>> {
        Proxy::new(connection, PORTAL_DEST, PORTAL_PATH, PORTAL_IFACE)
    }

    fn publish(flag: &AtomicBool, ctx: &egui::Context, value: bool) {
        if flag.swap(value, Ordering::Release) != value {
            ctx.request_repaint();
        }
    }

    fn read_portal(proxy: &Proxy<'_>) -> bool {
        let portal_reduced = read_setting(proxy, "org.freedesktop.appearance", "reduced-motion")
            .is_some_and(|value| portal_u32(&value) == Some(1));
        let animations_off =
            read_setting(proxy, "org.gnome.desktop.interface", "enable-animations")
                .is_some_and(|value| portal_bool(&value) == Some(false));
        sources_reduce(MotionSources {
            portal_reduced,
            animations_off,
            kde_instant: kde_file_is_instant(),
        })
    }

    /// `ReadOne` on a current portal, then the older `Read`.
    fn read_setting(proxy: &Proxy<'_>, namespace: &str, key: &str) -> Option<OwnedValue> {
        call_value(proxy, "ReadOne", namespace, key)
            .or_else(|| call_value(proxy, "Read", namespace, key))
    }

    fn call_value(
        proxy: &Proxy<'_>,
        method: &str,
        namespace: &str,
        key: &str,
    ) -> Option<OwnedValue> {
        proxy.call(method, &(namespace, key)).ok()
    }

    fn motion_event(message: &zbus::message::Message) -> bool {
        let Ok((namespace, key, _value)) =
            message.body().deserialize::<(String, String, OwnedValue)>()
        else {
            return false;
        };
        setting_event_is_motion(&namespace, &key)
    }

    fn portal_u32(value: &OwnedValue) -> Option<u32> {
        match peel(value) {
            Value::U8(n) => Some(u32::from(*n)),
            Value::U16(n) => Some(u32::from(*n)),
            Value::U32(n) => Some(*n),
            Value::U64(n) => u32::try_from(*n).ok(),
            Value::I32(n) => u32::try_from(*n).ok(),
            _ => None,
        }
    }

    fn portal_bool(value: &OwnedValue) -> Option<bool> {
        match peel(value) {
            Value::Bool(n) => Some(*n),
            _ => None,
        }
    }

    fn peel<'a>(value: &'a OwnedValue) -> &'a Value<'a> {
        let mut current: &Value<'_> = value;
        while let Value::Value(inner) = current {
            current = inner;
        }
        current
    }

    fn kde_file_is_instant() -> bool {
        let Some(path) = kdeglobals_path() else {
            return false;
        };
        let Ok(meta) = std::fs::metadata(&path) else {
            return false;
        };
        if meta.len() > KDEGLOBALS_LIMIT {
            return false;
        }
        std::fs::read_to_string(&path).is_ok_and(|text| kde_animation_factor_is_zero(&text))
    }

    fn kdeglobals_path() -> Option<std::path::PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(std::path::PathBuf::from)
            .filter(|path| !path.as_os_str().is_empty())
            .or_else(|| {
                std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".config"))
            })?;
        Some(base.join("kdeglobals"))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn owned(value: Value<'static>) -> OwnedValue {
            OwnedValue::try_from(value).expect("owned")
        }

        #[test]
        fn a_portal_value_of_one_means_reduce_and_other_values_do_not() {
            assert_eq!(portal_u32(&owned(Value::U32(1))), Some(1));
            assert_eq!(portal_u32(&owned(Value::U32(0))), Some(0));
            assert_eq!(portal_u32(&owned(Value::U32(2))), Some(2));
            let wrapped = owned(Value::Value(Box::new(Value::Value(Box::new(Value::U32(
                1,
            ))))));
            assert_eq!(portal_u32(&wrapped), Some(1));
            assert_eq!(portal_bool(&owned(Value::Bool(false))), Some(false));
            assert_eq!(portal_bool(&owned(Value::Bool(true))), Some(true));
            assert_eq!(portal_u32(&owned(Value::Bool(true))), None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn any_source_that_asks_for_less_motion_wins() {
        let quiet = MotionSources {
            portal_reduced: false,
            animations_off: false,
            kde_instant: false,
        };
        assert!(!sources_reduce(quiet));
        assert!(sources_reduce(MotionSources {
            portal_reduced: true,
            ..quiet
        }));
        assert!(sources_reduce(MotionSources {
            animations_off: true,
            ..quiet
        }));
        assert!(sources_reduce(MotionSources {
            kde_instant: true,
            ..quiet
        }));
    }

    #[test]
    fn only_a_motion_setting_change_wakes_a_reread() {
        assert!(setting_event_is_motion(
            "org.freedesktop.appearance",
            "reduced-motion"
        ));
        assert!(setting_event_is_motion(
            "org.gnome.desktop.interface",
            "enable-animations"
        ));
        assert!(setting_event_is_motion(
            "org.gnome.desktop.a11y.interface",
            "reduced-motion"
        ));
        assert!(setting_event_is_motion(
            "org.kde.kdeglobals.KDE",
            "AnimationDurationFactor"
        ));
        assert!(!setting_event_is_motion(
            "org.freedesktop.appearance",
            "color-scheme"
        ));
    }

    #[test]
    fn a_zero_kde_animation_factor_means_reduce() {
        assert!(kde_animation_factor_is_zero(
            "[KDE]\nAnimationDurationFactor=0\n"
        ));
        assert!(kde_animation_factor_is_zero(
            "\n[Other]\nAnimationDurationFactor=0\n[KDE]\nAnimationDurationFactor=0.0\n"
        ));
        assert!(!kde_animation_factor_is_zero(
            "[KDE]\nAnimationDurationFactor=1\n"
        ));
        assert!(!kde_animation_factor_is_zero(
            "[KDE]\nAnimationDurationFactor=0.5\n"
        ));
        assert!(!kde_animation_factor_is_zero(
            "[General]\nAnimationDurationFactor=0\n"
        ));
        assert!(!kde_animation_factor_is_zero(""));
        assert!(!kde_animation_factor_is_zero(
            "[KDE]\n# AnimationDurationFactor=0\n"
        ));
    }

    #[test]
    fn linux_startup_waits_a_short_bounded_time() {
        assert_eq!(LINUX_FIRST_READ, Duration::from_millis(200));
    }
}
