//! Route TDLib updates to workers by client id.
//!
//! TDLib forbids two threads in `td_receive` at the same time, and the
//! returned string is valid only until the next call. So one process-wide
//! thread receives, and this router hands each update to the worker that
//! owns its client id. No TDLib types here, so default builds test it.

#![cfg_attr(not(feature = "telegram-tdlib"), allow(dead_code))]

use std::collections::HashMap;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// Client id → worker channel, plus "a receive call is in flight".
#[derive(Debug)]
pub(super) struct Router<U> {
    routes: HashMap<i32, UnboundedSender<U>>,
    receiving: bool,
}

impl<U> Router<U> {
    pub(super) fn new() -> Self {
        Self {
            routes: HashMap::new(),
            receiving: false,
        }
    }

    /// Register a client before its first request, so no update is missed.
    pub(super) fn register(&mut self, client_id: i32) -> UnboundedReceiver<U> {
        let (sender, receiver) = unbounded_channel();
        self.routes.insert(client_id, sender);
        receiver
    }

    pub(super) fn unregister(&mut self, client_id: i32) {
        self.routes.remove(&client_id);
    }

    /// Hand `update` to its worker. Returns `false` when no live worker owns
    /// the client id; a closed channel drops its route.
    pub(super) fn route(&mut self, client_id: i32, update: U) -> bool {
        let Some(sender) = self.routes.get(&client_id) else {
            return false;
        };
        if sender.send(update).is_ok() {
            return true;
        }
        self.routes.remove(&client_id);
        false
    }

    /// The receive thread may call `td_receive` only while a client exists.
    pub(super) fn begin_receive(&mut self) -> bool {
        if self.routes.is_empty() {
            return false;
        }
        self.receiving = true;
        true
    }

    pub(super) fn end_receive(&mut self) {
        self.receiving = false;
    }

    /// No client and no receive call in flight: no thread is inside TDLib.
    pub(super) fn idle(&self) -> bool {
        self.routes.is_empty() && !self.receiving
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updates_reach_only_the_worker_that_owns_the_client() {
        let mut router = Router::new();
        let mut old = router.register(1);
        let mut new = router.register(2);
        assert!(router.route(1, "closed"));
        assert!(router.route(2, "wait-phone"));
        assert!(router.route(1, "late"));
        assert_eq!(old.try_recv().ok(), Some("closed"));
        assert_eq!(old.try_recv().ok(), Some("late"));
        assert_eq!(new.try_recv().ok(), Some("wait-phone"));
        assert!(
            new.try_recv().is_err(),
            "no update crosses to another client"
        );
        assert!(!router.route(3, "unknown"));
    }

    #[test]
    fn a_gone_worker_loses_its_route() {
        let mut router = Router::new();
        let gone = router.register(1);
        drop(gone);
        assert!(!router.route(1, 0));
        assert!(!router.route(1, 0));
        let _kept = router.register(2);
        router.unregister(2);
        assert!(!router.route(2, 0));
    }

    #[test]
    fn receive_runs_only_while_a_client_exists_and_idle_waits_for_it() {
        let mut router: Router<u8> = Router::new();
        assert!(router.idle());
        assert!(!router.begin_receive(), "no client, no td_receive");
        let _worker = router.register(1);
        assert!(!router.idle());
        assert!(router.begin_receive());
        router.unregister(1);
        assert!(!router.idle(), "a receive call is still in flight");
        router.end_receive();
        assert!(router.idle());
        assert!(!router.begin_receive());
    }
}
