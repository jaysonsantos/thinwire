//! macOS Dock icon while thinwire closes in the background.
//!
//! The first close hides the window, but the process lives until the
//! clients stopped (up to the close limit plus the watchdog margin). A
//! regular macOS app keeps its Dock icon and its Cmd-Tab entry for that
//! time. The accessory activation policy removes both at once. The
//! process still runs and finishes its shutdown.

/// Take the app out of the Dock and Cmd-Tab. Call it on the main thread:
/// eframe runs `logic()` there. A no-op on other platforms.
pub(crate) fn hide_icon() {
    #[cfg(target_os = "macos")]
    macos::hide_icon();
}

#[cfg(target_os = "macos")]
mod macos {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

    pub(super) fn hide_icon() {
        // AppKit allows the policy change on the main thread only.
        let Some(main_thread) = MainThreadMarker::new() else {
            tracing::warn!("dock icon not hidden: not on the main thread");
            return;
        };
        let app = NSApplication::sharedApplication(main_thread);
        if !app.setActivationPolicy(NSApplicationActivationPolicy::Accessory) {
            tracing::warn!("dock icon not hidden: macOS refused the accessory policy");
        }
    }
}

#[cfg(test)]
mod tests {
    /// Off macOS the call does nothing and needs no main thread.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn hiding_the_dock_icon_is_a_no_op_off_macos() {
        std::thread::spawn(super::hide_icon)
            .join()
            .expect("no panic off the main thread");
    }
}
