//! macOS: UNUserNotificationCenter through `mac-usernotifications` (#161).
//!
//! Each chat has one request identifier (the tag), so a new message
//! replaces its notification. A dismiss removes it, and a click on the body
//! opens the chat. The center needs a bundle id and a code signature (an
//! ad-hoc one is enough): `Thinwire.app` has both (ADR 0003). `cargo run`
//! has no bundle and keeps the show-only backend. macOS 11 also keeps it.
//! `setInterruptionLevel:` is a macOS 12 selector: the call is skipped
//! there (`interruption_level_on`), because it aborts the process.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use mac_usernotifications as un;
use mac_usernotifications::block_on;

use crate::tagged::{StartupRemoval, TagService, listed, startup_removals, update_sends};
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
        // waiter opens their chat now. Pending requests are cancelled too,
        // and each call finishes before the next: the blocking helpers
        // return before macOS runs them, so a pending request could still
        // be delivered after this function returned (Codex r4139073814).
        for step in startup_removals() {
            match step {
                StartupRemoval::CancelPending => {
                    for id in block_on(un::get_pending_notification_ids()) {
                        block_on(un::cancel_pending(&id));
                    }
                }
                StartupRemoval::CloseDelivered => {
                    for id in block_on(un::get_delivered_notification_ids()) {
                        block_on(un::close_delivered(&id));
                    }
                }
            }
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
        let mut request = un::Notification::new()
            .id(tag)
            .title(&notification.title)
            .message(notification.body());
        // macOS 11 has no `setInterruptionLevel:`. Calling it aborts the
        // process. `LSMinimumSystemVersion` stays 11.0, so the guard is
        // here, at the call (Codex r4138724926).
        if crate::interruption_level_on(os_major()) {
            let level = if quiet {
                un::InterruptionLevel::Passive
            } else {
                un::InterruptionLevel::Active
            };
            request = request.interruption_level(level);
        }
        if !quiet {
            request = request.default_sound();
        }
        // Check again here, immediately before send. Tagged::update already
        // asked `has`, but the user can clear the notification in between.
        // send with this id then creates a new one. An update must not
        // (Codex r4139322212). A small window remains between the second
        // pending/delivered check and `send()`, so `update_sends` is not
        // a guarantee.
        if quiet {
            let pending = block_on(un::get_pending_notification_ids());
            let delivered = block_on(un::get_delivered_notification_ids());
            if !update_sends(tag, &delivered, &pending) {
                return Ok(());
            }
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
        // Pending first: a request that is delivered between the two reads
        // is then in the delivered list. In the other order it is in
        // neither list and looks absent.
        let pending = block_on(un::get_pending_notification_ids());
        let delivered = block_on(un::get_delivered_notification_ids());
        Ok(listed(tag, &delivered, &pending))
    }

    /// Cancel a pending request and wait for macOS to run that, then close
    /// a delivered one. The blocking helpers return before the worker runs
    /// them, so a close can miss a request that is still pending and leave
    /// it behind once it is delivered (same as startup cleanup).
    fn remove(&mut self, tag: &str) -> Result<(), BackendError> {
        block_on(un::cancel_pending(tag));
        block_on(un::close_delivered(tag));
        Ok(())
    }
}
