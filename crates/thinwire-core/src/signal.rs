//! Change signal: the core bumps a revision, frontends wait on it.
//!
//! The signal carries no state. It only says "read the view again". A
//! frontend that wakes late sees one change, not a queue of them.

use std::sync::Arc;

use tokio::sync::watch;

/// Producer side. The core and its workers hold clones of it.
#[derive(Debug, Clone)]
pub struct ChangeNotifier {
    tx: Arc<watch::Sender<u64>>,
}

impl ChangeNotifier {
    /// Tell every waiting frontend that the view changed.
    ///
    /// Never blocks and never fails. A frontend that is gone is ignored.
    pub fn notify(&self) {
        self.tx
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }

    /// A new consumer. It sees the current revision as already read.
    #[must_use]
    pub fn subscribe(&self) -> ChangeSignal {
        ChangeSignal {
            rx: self.tx.subscribe(),
        }
    }
}

/// Consumer side. One per frontend loop or repaint task.
#[derive(Debug, Clone)]
pub struct ChangeSignal {
    rx: watch::Receiver<u64>,
}

impl ChangeSignal {
    /// Wait for the next change. Returns false when the core is gone.
    pub async fn changed(&mut self) -> bool {
        self.rx.changed().await.is_ok()
    }

    /// True when a change came after the last read. Does not wait.
    #[must_use]
    pub fn has_changed(&self) -> bool {
        self.rx.has_changed().unwrap_or(false)
    }

    /// Mark the current revision as read and return it.
    pub fn mark_seen(&mut self) -> u64 {
        *self.rx.borrow_and_update()
    }
}

/// Make a connected notifier and signal pair.
#[must_use]
pub fn change_channel() -> (ChangeNotifier, ChangeSignal) {
    let (tx, rx) = watch::channel(0);
    (ChangeNotifier { tx: Arc::new(tx) }, ChangeSignal { rx })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn notify_sets_has_changed_until_seen() {
        let (notifier, mut signal) = change_channel();
        assert!(!signal.has_changed());
        notifier.notify();
        notifier.notify();
        assert!(signal.has_changed());
        assert_eq!(signal.mark_seen(), 2);
        assert!(!signal.has_changed());
    }

    #[tokio::test]
    async fn changed_wakes_a_waiting_frontend() {
        let (notifier, mut signal) = change_channel();
        let waiter = tokio::spawn(async move { signal.changed().await });
        tokio::time::sleep(Duration::from_millis(5)).await;
        notifier.clone().notify();
        assert!(waiter.await.expect("waiter task"));
    }

    #[tokio::test]
    async fn changed_returns_false_when_the_core_is_gone() {
        let (notifier, mut signal) = change_channel();
        drop(notifier);
        assert!(!signal.changed().await);
    }

    #[test]
    fn late_subscriber_starts_clean() {
        let (notifier, _first) = change_channel();
        notifier.notify();
        let late = notifier.subscribe();
        assert!(!late.has_changed());
    }
}
