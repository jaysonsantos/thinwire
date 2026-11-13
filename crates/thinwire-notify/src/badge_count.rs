//! The unread count on the icon of the app (#513).
//!
//! The core gives the count (`thinwire_core::View::badge_count`). This
//! module shows it:
//! - macOS: the badge of the Dock icon (`NSDockTile`). AppKit allows the
//!   call on the main thread only, so [`BadgeCount::set`] runs there.
//! - Linux: the signal `Update` of `com.canonical.Unity.LauncherEntry` on
//!   the session bus, for `application://thinwire.desktop`. A launcher
//!   that knows this interface shows the count on the entry of that
//!   desktop file (KDE Plasma, Dash to Dock, Unity). A thread sends the
//!   signal, so no D-Bus call runs on the UI thread.
//! - Windows: no badge yet. A taskbar overlay icon needs
//!   `ITaskbarList3`, and that call is not checked on Windows. `set` does
//!   nothing there, and [`BadgeCount::SUPPORTED`] is `false`.
//!
//! A count of 0 removes the badge. No log line has more than a fixed text.

use std::time::Duration;

#[cfg(target_os = "macos")]
mod dock;
#[cfg(target_os = "linux")]
mod launcher;

/// The unread count on the Dock icon or the launcher entry.
#[derive(Default)]
pub struct BadgeCount {
    /// The count that the OS has now. `None`: nothing was sent yet.
    last: Option<u32>,
    #[cfg(target_os = "linux")]
    launcher: launcher::Launcher,
}

impl BadgeCount {
    /// This OS can show the count. A frontend hides its switch when not.
    pub const SUPPORTED: bool = cfg!(any(target_os = "macos", target_os = "linux"));

    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Show `count`, or remove the badge for 0. It does nothing when the
    /// OS already has this count. Call it on the main thread: macOS needs
    /// that. On Linux it only queues the count for the D-Bus thread.
    pub fn set(&mut self, count: u32) {
        if changed(&mut self.last, count) {
            #[cfg(target_os = "macos")]
            dock::show(count);
            #[cfg(target_os = "linux")]
            self.launcher.send(count);
        }
    }

    /// Remove the badge at shutdown, and wait at most `limit` until the
    /// OS has the change. Call it on the main thread. With no count sent
    /// in this run, it does nothing.
    pub fn clear(&mut self, limit: Duration) {
        if self.last.is_none() {
            return;
        }
        self.set(0);
        #[cfg(target_os = "linux")]
        self.launcher.flush(limit);
        #[cfg(not(target_os = "linux"))]
        let _ = limit;
    }
}

/// Keep `count` as the newest one. `true` when the OS does not have it.
/// The first count is always new: it also removes a badge that a run
/// with no clean exit left on the launcher.
fn changed(last: &mut Option<u32>, count: u32) -> bool {
    if *last == Some(count) {
        return false;
    }
    *last = Some(count);
    true
}

/// The text of the Dock badge. `None` removes the badge.
#[cfg(any(target_os = "macos", test))]
fn label(count: u32) -> Option<String> {
    (count > 0).then(|| count.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_new_count_goes_to_the_os() {
        let mut last = None;
        assert!(changed(&mut last, 0), "the first count clears an old badge");
        assert!(!changed(&mut last, 0));
        assert!(changed(&mut last, 3));
        assert!(!changed(&mut last, 3));
        assert!(changed(&mut last, 0));
        assert_eq!(last, Some(0));
    }

    #[test]
    fn zero_has_no_label() {
        assert_eq!(label(0), None);
        assert_eq!(label(1).as_deref(), Some("1"));
        assert_eq!(label(1234).as_deref(), Some("1234"));
    }

    #[test]
    fn a_clear_with_no_count_sent_does_nothing() {
        let mut badge = BadgeCount::new();
        badge.clear(Duration::ZERO);
        assert_eq!(badge.last, None);
    }

    #[test]
    fn windows_has_no_badge_yet() {
        assert_eq!(
            BadgeCount::SUPPORTED,
            cfg!(any(target_os = "macos", target_os = "linux"))
        );
    }
}
