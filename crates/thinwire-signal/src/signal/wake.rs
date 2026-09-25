// SPDX-License-Identifier: AGPL-3.0-only
#![cfg_attr(not(feature = "signal-local"), allow(dead_code))]
//! One cancel token per link attempt. A later attempt does not see an old permit.

use std::sync::Arc;

/// Cancel signal for a single attempt. Drop it with the attempt.
#[derive(Debug)]
pub(crate) struct AttemptCancel {
    notify: Arc<tokio::sync::Notify>,
}

impl AttemptCancel {
    pub(crate) fn new() -> Self {
        Self {
            notify: Arc::new(tokio::sync::Notify::new()),
        }
    }

    pub(crate) fn handle(&self) -> Arc<tokio::sync::Notify> {
        Arc::clone(&self.notify)
    }

    /// Wake only this attempt.
    pub(crate) fn cancel(&self) {
        self.notify.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    #[tokio::test]
    async fn a_cancel_wakes_only_that_attempt() {
        let first = AttemptCancel::new();
        let waiting = first.handle();
        let started = Instant::now();
        let handle = tokio::spawn(async move {
            waiting.notified().await;
        });
        first.cancel();
        handle.await.expect("worker");
        assert!(started.elapsed() < Duration::from_secs(2));

        let retry = AttemptCancel::new();
        let pending =
            tokio::time::timeout(Duration::from_millis(50), retry.handle().notified()).await;
        assert!(
            pending.is_err(),
            "a new attempt does not see the old cancel"
        );
    }
}
