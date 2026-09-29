//! macOS: UNUserNotificationCenter through `mac-usernotifications` (#161).
//!
//! Each chat has one request identifier (the tag), so a new message
//! replaces its notification. A dismiss removes it, and a click on the body
//! opens the chat. The center needs a bundle id and a code signature (an
//! ad-hoc one is enough): `Thinwire.app` has both (ADR 0003). `cargo run`
//! has no bundle and keeps the show-only backend. macOS 11 also keeps it:
//! the crate uses a macOS 12 API (see `MIN_CENTER_MACOS`).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use mac_usernotifications as un;
use mac_usernotifications::block_on;

use crate::tagged::TagService;
use crate::{BackendError, ClickFn, Notification};

/// Most click-waiter threads at one time, as on Linux. Over the limit, a
/// notification shows with no click.
const MAX_CLICK_WAITERS: usize = 16;

pub(crate) struct Center {
    clicks: ClickFn,
    waiters: Arc<AtomicUsize>,
}

impl Center {
    /// `None` when the process has no bundle id (`cargo run`). Runs on the
    /// notification thread: the permission prompt does not block the UI.
    pub(crate) fn new(clicks: ClickFn) -> Option<Self> {
        un::check_bundle().ok()?;
        // A "no" keeps the center: the user can allow notifications later in
        // System Settings, and macOS applies that at once.
        match un::blocking::request_auth() {
            Ok(true) => {}
            Ok(false) => tracing::info!("notifications are off for thinwire in System Settings"),
            Err(_) => tracing::warn!("could not ask for notification permission"),
        }
        // Notifications of an earlier run that did not end cleanly: no
        // waiter opens their chat now.
        for id in block_on(un::get_delivered_notification_ids()) {
            un::blocking::close_delivered(&id);
        }
        Some(Self {
            clicks,
            waiters: Arc::new(AtomicUsize::new(0)),
        })
    }
}

/// Major version of the running macOS, for example 26. Safe API, no
/// `unsafe` (the workspace forbids it).
pub(crate) fn os_major() -> isize {
    objc2_foundation::NSProcessInfo::processInfo()
        .operatingSystemVersion()
        .majorVersion
}

/// The user did not allow notifications (yet). macOS then rejects each
/// request: that is the user's choice, not a failure to retry.
fn not_allowed() -> bool {
    matches!(
        block_on(un::get_notification_settings()).map(|settings| settings.authorization_status),
        Ok(un::AuthorizationStatus::Denied | un::AuthorizationStatus::NotDetermined)
    )
}

impl TagService for Center {
    fn post(
        &mut self,
        tag: &str,
        notification: &Notification,
        quiet: bool,
    ) -> Result<(), BackendError> {
        let level = if quiet {
            un::InterruptionLevel::Passive
        } else {
            un::InterruptionLevel::Active
        };
        let mut request = un::Notification::new()
            .id(tag)
            .title(&notification.title)
            .message(notification.body())
            .interruption_level(level);
        if !quiet {
            request = request.default_sound();
        }
        let handle = match block_on(request.send()) {
            Ok(handle) => handle,
            Err(_) if not_allowed() => return Ok(()),
            Err(_) => return Err(BackendError("show")),
        };
        if self.waiters.load(Ordering::SeqCst) >= MAX_CLICK_WAITERS {
            return Ok(());
        }
        // A replace or a dismiss ends the waiter of the older request.
        let clicks = Arc::clone(&self.clicks);
        let key = notification.key.clone();
        let waiters = Arc::clone(&self.waiters);
        waiters.fetch_add(1, Ordering::SeqCst);
        let spawned = thread::Builder::new()
            .name("thinwire-notify-click".into())
            .spawn(move || {
                if block_on(handle.response()).is_ok_and(|response| response.is_default_action()) {
                    clicks(key);
                }
                waiters.fetch_sub(1, Ordering::SeqCst);
            });
        if spawned.is_err() {
            self.waiters.fetch_sub(1, Ordering::SeqCst);
        }
        Ok(())
    }

    fn has(&mut self, tag: &str) -> Result<bool, BackendError> {
        Ok(block_on(un::get_delivered_notification_ids())
            .iter()
            .any(|id| id == tag))
    }

    fn remove(&mut self, tag: &str) -> Result<(), BackendError> {
        un::blocking::close_delivered(tag);
        Ok(())
    }
}
