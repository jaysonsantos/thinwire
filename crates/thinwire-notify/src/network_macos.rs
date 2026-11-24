//! The cost of the network on macOS: a path monitor of Network.framework.
//!
//! macOS tells the cost only through Network.framework, which has a C API
//! with blocks and no safe Rust binding. This module is the one place where
//! the app calls it, so it is the second module of the workspace with
//! `unsafe` code. `scripts/check-unsafe.sh` keeps the list at two.
//!
//! The monitor runs for the life of the app on a private dispatch queue.
//! Each new path goes to the handler: a path that is "expensive" (a phone
//! network or a hotspot) or "constrained" (Low Data Mode) is metered. A path
//! that is not satisfied has an unknown cost.

#![allow(unsafe_code)]

use std::ffi::{c_char, c_void};
use std::sync::mpsc::Sender;

use block2::RcBlock;
use thinwire_core::NetworkCost;

/// An `nw_path_monitor_t`, `nw_path_t`, or `dispatch_queue_t`: an opaque
/// object of the OS.
type Object = *mut c_void;

/// `nw_path_status_satisfied`.
const STATUS_SATISFIED: i32 = 1;

#[link(name = "Network", kind = "framework")]
unsafe extern "C" {
    fn nw_path_monitor_create() -> Object;
    fn nw_path_monitor_set_queue(monitor: Object, queue: Object);
    fn nw_path_monitor_set_update_handler(monitor: Object, handler: &block2::Block<dyn Fn(Object)>);
    fn nw_path_monitor_start(monitor: Object);
    fn nw_path_get_status(path: Object) -> i32;
    fn nw_path_is_expensive(path: Object) -> bool;
    fn nw_path_is_constrained(path: Object) -> bool;
}

// libdispatch is part of libSystem, which every program links.
unsafe extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> Object;
}

/// The cost of a path, from its three flags.
#[must_use]
pub(crate) fn cost_of_path(satisfied: bool, expensive: bool, constrained: bool) -> NetworkCost {
    if !satisfied {
        NetworkCost::Unknown
    } else if expensive || constrained {
        NetworkCost::Metered
    } else {
        NetworkCost::Unmetered
    }
}

/// Start the monitor. Each new path sends its cost and runs `wake`. The
/// monitor, its queue, and its handler stay until the process ends: the
/// app reads the cost for its whole life. `false` when the OS gave no
/// monitor or no queue.
pub(crate) fn start(send: Sender<NetworkCost>, wake: impl Fn() + Send + 'static) -> bool {
    let handler = RcBlock::new(move |path: Object| {
        if path.is_null() {
            return;
        }
        // SAFETY: `path` is the live `nw_path_t` that Network.framework
        // passes to the handler; it stays valid for this call.
        let cost = unsafe {
            cost_of_path(
                nw_path_get_status(path) == STATUS_SATISFIED,
                nw_path_is_expensive(path),
                nw_path_is_constrained(path),
            )
        };
        // The app ended: nothing reads the cost any more.
        if send.send(cost).is_ok() {
            wake();
        }
    });
    // SAFETY: plain C calls with valid arguments. The label is a C string
    // with a final NUL, and a null attribute makes a serial queue.
    let (monitor, queue) = unsafe {
        (
            nw_path_monitor_create(),
            dispatch_queue_create(c"thinwire-network".as_ptr(), std::ptr::null()),
        )
    };
    if monitor.is_null() || queue.is_null() {
        return false;
    }
    // SAFETY: `monitor` and `queue` are new live objects. The monitor copies
    // the handler block. The three objects are not released: they live as
    // long as the process.
    unsafe {
        nw_path_monitor_set_queue(monitor, queue);
        nw_path_monitor_set_update_handler(monitor, &handler);
        nw_path_monitor_start(monitor);
    }
    std::mem::forget(handler);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_phone_network_or_low_data_mode_is_metered() {
        assert_eq!(cost_of_path(true, false, false), NetworkCost::Unmetered);
        assert_eq!(cost_of_path(true, true, false), NetworkCost::Metered);
        assert_eq!(cost_of_path(true, false, true), NetworkCost::Metered);
        assert_eq!(cost_of_path(false, false, false), NetworkCost::Unknown);
    }

    /// The monitor of this Mac answers with one cost soon after the start.
    #[test]
    fn the_monitor_sends_a_first_cost() {
        let (send, costs) = std::sync::mpsc::channel();
        assert!(start(send, || {}));
        let first = costs.recv_timeout(std::time::Duration::from_secs(10));
        assert!(first.is_ok(), "no path from Network.framework");
    }
}
