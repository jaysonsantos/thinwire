//! The badge of the macOS Dock icon.

use objc2::MainThreadMarker;
use objc2_app_kit::NSApplication;
use objc2_foundation::NSString;

/// Set the badge text of the Dock tile. 0 removes the badge.
pub(super) fn show(count: u32) {
    // AppKit allows the Dock tile on the main thread only.
    let Some(main_thread) = MainThreadMarker::new() else {
        tracing::warn!("dock badge not set: not on the main thread");
        return;
    };
    let label = super::label(count).map(|text| NSString::from_str(&text));
    NSApplication::sharedApplication(main_thread)
        .dockTile()
        .setBadgeLabel(label.as_deref());
}
