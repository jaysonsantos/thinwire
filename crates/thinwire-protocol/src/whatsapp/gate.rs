//! Serializes WhatsApp bot registration with shutdown.
//!
//! `run_link` can sit between `Bot::spawn` and storing the handle. Shutdown
//! must not treat that gap as an idle link: it waits until every older start
//! has either stored its bot or dropped it, then takes the bot. A later
//! `publish` of that same start cannot install after the wait.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tokio::sync::{Mutex, Notify};

struct Slot<T> {
    /// Tokens that have entered and have not published or left.
    inflight: Vec<u64>,
    handle: Option<(u64, T)>,
}

pub(super) struct LinkGate<T> {
    generation: AtomicU64,
    active: AtomicBool,
    slot: Mutex<Slot<T>>,
    /// Wakes [`Self::shutdown`] when an in-flight start publishes or leaves.
    ///
    /// `notify_waiters` (not `notify_one`): the waiter is created while the
    /// slot lock is held, and `notify_waiters` reaches it even before it is
    /// polled. A permit from `notify_one` would not.
    idle: Notify,
    #[cfg(test)]
    waiting: AtomicU64,
}

#[cfg_attr(not(feature = "whatsapp-web"), allow(dead_code))]
impl<T> LinkGate<T> {
    pub(super) fn new() -> Self {
        Self {
            generation: AtomicU64::new(0),
            active: AtomicBool::new(false),
            slot: Mutex::new(Slot {
                inflight: Vec::new(),
                handle: None,
            }),
            idle: Notify::new(),
            #[cfg(test)]
            waiting: AtomicU64::new(0),
        }
    }

    pub(super) fn next_generation(&self) -> u64 {
        self.active.store(false, Ordering::SeqCst);
        self.generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub(super) fn mark_active(&self) {
        self.active.store(true, Ordering::SeqCst);
    }

    pub(super) fn set_inactive(&self) {
        self.active.store(false, Ordering::SeqCst);
    }

    pub(super) fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    pub(super) fn is_current(&self, token: u64) -> bool {
        self.generation.load(Ordering::SeqCst) == token
    }

    /// Count `token` until [`Self::publish`] stores a bot or [`Self::leave`].
    ///
    /// `false` when `token` is already stale. The caller must not spawn a bot.
    pub(super) async fn enter(&self, token: u64) -> bool {
        let mut slot = self.slot.lock().await;
        if !self.is_current(token) {
            return false;
        }
        slot.inflight.push(token);
        true
    }

    /// Drop `token` after a start that did not store a bot.
    ///
    /// A rejected bot is closed by the caller before this, so shutdown cannot
    /// finish while that bot is still running.
    pub(super) async fn leave(&self, token: u64) {
        let mut slot = self.slot.lock().await;
        self.remove_token(&mut slot, token);
    }

    /// Install `handle` if `token` is still the current generation.
    ///
    /// `Ok(previous)` stores the bot and ends the flight. The caller closes
    /// `previous` if it is `Some`.
    ///
    /// `Err(handle)` does not store and does not end the flight. The caller
    /// closes `handle`, then [`Self::leave`].
    pub(super) async fn publish(&self, token: u64, handle: T) -> Result<Option<T>, T> {
        let mut slot = self.slot.lock().await;
        if !self.is_current(token) {
            return Err(handle);
        }
        let previous = slot.handle.replace((token, handle)).map(|(_, old)| old);
        self.remove_token(&mut slot, token);
        Ok(previous)
    }

    /// Take a stored bot from an older generation so a new start can replace it.
    pub(super) async fn take_older(&self, token: u64) -> Option<T> {
        let mut slot = self.slot.lock().await;
        match slot.handle.take() {
            Some((generation, handle)) if generation < token => Some(handle),
            other => {
                slot.handle = other;
                None
            }
        }
    }

    /// Wait out starts older than the current generation, then take that bot.
    ///
    /// Callers advance the generation (`next_generation`) before this. A start
    /// still between spawn and `publish` is in flight, so this does not return
    /// — and `Stopped` is not emitted — while that start can still install a
    /// bot. A newer generation's bot stays stored.
    pub(super) async fn shutdown(&self) -> Option<T> {
        self.active.store(false, Ordering::SeqCst);
        let seen = self.generation.load(Ordering::SeqCst);
        loop {
            let mut slot = self.slot.lock().await;
            if !slot.inflight.iter().any(|token| *token < seen) {
                return match slot.handle.take() {
                    Some((generation, handle)) if generation < seen => Some(handle),
                    other => {
                        slot.handle = other;
                        None
                    }
                };
            }
            // Create the waiter before releasing the lock. `leave` / `publish`
            // notify under the same lock, so the wakeup cannot land in between.
            #[cfg(test)]
            self.waiting.fetch_add(1, Ordering::SeqCst);
            let notified = self.idle.notified();
            drop(slot);
            notified.await;
            #[cfg(test)]
            self.waiting.fetch_sub(1, Ordering::SeqCst);
        }
    }

    fn remove_token(&self, slot: &mut Slot<T>, token: u64) {
        let Some(index) = slot.inflight.iter().position(|item| *item == token) else {
            return;
        };
        slot.inflight.swap_remove(index);
        self.idle.notify_waiters();
    }

    #[cfg(test)]
    async fn stored(&self) -> Option<T>
    where
        T: Copy,
    {
        self.slot.lock().await.handle.map(|(_, value)| value)
    }

    #[cfg(test)]
    fn waiting(&self) -> u64 {
        self.waiting.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;
    use std::task::Poll;

    use super::*;

    async fn poll_once<F: Future>(mut fut: Pin<&mut F>) -> Poll<F::Output> {
        std::future::poll_fn(|cx| Poll::Ready(fut.as_mut().poll(cx))).await
    }

    /// Codex P2: shutdown observed no handle in the gap after the last
    /// generation check and before the store, emitted Stopped, then `run_link`
    /// installed the bot. Shutdown must stay pending for that whole gap, and
    /// the late publish must not stick.
    #[tokio::test]
    async fn shutdown_waits_out_a_start_between_spawn_and_store() {
        let gate = LinkGate::new();
        let token = gate.next_generation();
        assert!(gate.enter(token).await);
        // Adapter shutdown bumps the generation before it waits.
        let _ = gate.next_generation();

        let mut shutdown = std::pin::pin!(gate.shutdown());
        let mut parked = false;
        for _ in 0..8 {
            let polled = poll_once(shutdown.as_mut()).await;
            assert!(
                polled.is_pending(),
                "shutdown decided the link was idle before the start published or left"
            );
            if gate.waiting() > 0 {
                parked = true;
                break;
            }
        }
        assert!(parked, "shutdown never waited for the in-flight start");

        assert!(
            gate.publish(token, "bot").await.is_err(),
            "a superseded start must not install the bot"
        );
        assert!(gate.stored().await.is_none());
        gate.leave(token).await;

        assert!(shutdown.await.is_none());
        assert!(gate.stored().await.is_none());
    }

    #[tokio::test]
    async fn shutdown_takes_a_bot_stored_before_close() {
        let gate = LinkGate::new();
        let token = gate.next_generation();
        assert!(gate.enter(token).await);
        assert!(gate.publish(token, "bot").await.is_ok());
        let _ = gate.next_generation();
        assert_eq!(gate.shutdown().await, Some("bot"));
        assert!(gate.stored().await.is_none());
    }

    #[tokio::test]
    async fn shutdown_leaves_a_newer_start_installed() {
        let gate = LinkGate::new();
        let old = gate.next_generation();
        assert!(gate.enter(old).await);
        let _ = gate.next_generation();
        let new = gate.next_generation();
        assert!(gate.enter(new).await);
        assert!(matches!(gate.publish(new, "new").await, Ok(None)));
        gate.leave(old).await;

        assert!(gate.shutdown().await.is_none());
        assert_eq!(gate.stored().await, Some("new"));
    }
}
