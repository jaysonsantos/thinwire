// SPDX-License-Identifier: AGPL-3.0-only
#![cfg_attr(not(feature = "signal-local"), allow(dead_code))]
//! Wakes a Signal worker that is blocked in `select`.

/// One permit wakes the receive loop so cancel does not wait out the join budget.
#[derive(Debug)]
pub(crate) struct CancelWake {
    notify: tokio::sync::Notify,
}

impl CancelWake {
    pub(crate) fn new() -> Self {
        Self {
            notify: tokio::sync::Notify::new(),
        }
    }

    pub(crate) fn wake(&self) {
        self.notify.notify_one();
    }

    /// Drop a permit that no waiter consumed. The next `cancelled` waits again.
    pub(crate) fn clear(&self) {
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        let mut pending = std::pin::pin!(self.notify.notified());
        let _ = pending.as_mut().poll(&mut cx);
    }

    pub(crate) async fn cancelled(&self) {
        self.notify.notified().await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::*;

    #[tokio::test]
    async fn a_cancel_wakes_the_idle_worker_at_once() {
        let wake = Arc::new(CancelWake::new());
        let waiting = Arc::clone(&wake);
        let started = Instant::now();
        let handle = tokio::spawn(async move {
            tokio::select! {
                biased;
                _ = waiting.cancelled() => {}
                _ = tokio::time::sleep(Duration::from_secs(30)) => {
                    panic!("idle worker waited for the deadline");
                }
            }
        });
        wake.wake();
        handle.await.expect("worker");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn a_cleared_permit_does_not_wake_the_next_wait() {
        let wake = CancelWake::new();
        wake.wake();
        wake.clear();
        let pending = tokio::time::timeout(Duration::from_millis(50), wake.cancelled()).await;
        assert!(pending.is_err(), "a stale cancel permit must not fire");
    }
}
