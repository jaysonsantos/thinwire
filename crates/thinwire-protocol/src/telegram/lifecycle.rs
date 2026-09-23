//! Worker bookkeeping: which TDLib workers are still closing.
//!
//! A new client must not open the TDLib database while an old client still
//! holds its lock. Each worker owns a done flag. The runtime keeps the flag
//! of the current worker and the flags of workers that are closing. No TDLib
//! types here, so default builds test it.

#![cfg_attr(not(feature = "telegram-tdlib"), allow(dead_code))]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Set by a worker when it has exited and released its client.
pub(super) type DoneFlag = Arc<AtomicBool>;

#[derive(Debug, Default)]
pub(super) struct WorkerSlots {
    current: Option<DoneFlag>,
    retiring: Vec<DoneFlag>,
}

impl WorkerSlots {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// The current worker is closing. The next worker must wait for it.
    pub(super) fn retire_current(&mut self) {
        if let Some(current) = self.current.take() {
            self.retiring.push(current);
        }
    }

    /// Start a new current worker. Returns its done flag and the flags it
    /// must wait for before it opens a client.
    pub(super) fn start(&mut self) -> (DoneFlag, Vec<DoneFlag>) {
        self.retire_current();
        self.retiring.retain(|flag| !is_done(flag));
        let done = Arc::new(AtomicBool::new(false));
        self.current = Some(Arc::clone(&done));
        (done, self.retiring.clone())
    }

    /// Every flag known now, current and retiring. Shutdown waits for these.
    pub(super) fn all(&self) -> Vec<DoneFlag> {
        self.retiring.iter().chain(&self.current).cloned().collect()
    }
}

#[must_use]
pub(super) fn is_done(flag: &DoneFlag) -> bool {
    flag.load(Ordering::SeqCst)
}

#[must_use]
pub(super) fn all_done(flags: &[DoneFlag]) -> bool {
    flags.iter().all(is_done)
}

pub(super) fn mark_done(flag: &DoneFlag) {
    flag.store(true, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_worker_waits_for_the_worker_that_is_closing() {
        let mut slots = WorkerSlots::new();
        let (first, wait) = slots.start();
        assert!(wait.is_empty(), "nothing to wait for at first");
        // Cancel → Add account: the first worker is closing, a second starts.
        slots.retire_current();
        let (second, wait) = slots.start();
        assert_eq!(wait.len(), 1);
        assert!(
            !all_done(&wait),
            "the first client still holds the database"
        );
        mark_done(&first);
        assert!(all_done(&wait));
        assert_eq!(slots.all().len(), 2);
        mark_done(&second);
        assert!(all_done(&slots.all()));
    }

    #[test]
    fn finished_workers_drop_out_of_the_wait_list() {
        let mut slots = WorkerSlots::new();
        let (first, _) = slots.start();
        mark_done(&first);
        let (_second, wait) = slots.start();
        assert!(wait.is_empty(), "a done worker is not waited for");
        slots.retire_current();
        assert_eq!(slots.all().len(), 1);
    }
}
