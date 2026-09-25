#![cfg_attr(not(feature = "signal-local"), allow(dead_code))]
//! Backoff after the Signal receive stream ends.
//!
//! An empty poll must not spin. The wait is sliced so shutdown can stop it.

use std::time::Duration;

/// First wait after the stream ends.
pub(crate) const RECONNECT_BACKOFF: Duration = Duration::from_secs(1);
/// Longest wait between receive attempts.
pub(crate) const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(30);
/// How often a wait checks that this generation is still current.
pub(crate) const RECONNECT_POLL: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamPoll {
    /// Keep reading this stream.
    Continue,
    /// The stream ended. Wait, then open it again.
    Reconnect { after: Duration },
    /// This generation was cancelled.
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReceiveLoop {
    backoff: Duration,
}

impl ReceiveLoop {
    pub(crate) const fn new() -> Self {
        Self {
            backoff: RECONNECT_BACKOFF,
        }
    }

    /// `item_ended` is the `None` from `Stream::next`. `still_current` is the
    /// worker generation check.
    pub(crate) fn on_item(&mut self, item_ended: bool, still_current: bool) -> StreamPoll {
        if !still_current {
            return StreamPoll::Stop;
        }
        if item_ended {
            let after = self.backoff;
            self.backoff = next_backoff(self.backoff);
            return StreamPoll::Reconnect { after };
        }
        self.backoff = RECONNECT_BACKOFF;
        StreamPoll::Continue
    }
}

pub(crate) fn next_backoff(current: Duration) -> Duration {
    let doubled = current.saturating_mul(2);
    if doubled > RECONNECT_BACKOFF_MAX {
        RECONNECT_BACKOFF_MAX
    } else {
        doubled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn end_of_stream_waits_and_does_not_spin() {
        let mut loop_ = ReceiveLoop::new();
        assert_eq!(
            loop_.on_item(true, true),
            StreamPoll::Reconnect {
                after: Duration::from_secs(1)
            }
        );
        assert_eq!(
            loop_.on_item(true, true),
            StreamPoll::Reconnect {
                after: Duration::from_secs(2)
            }
        );
        assert_ne!(loop_.on_item(true, true), StreamPoll::Continue);
        assert!(RECONNECT_BACKOFF >= RECONNECT_POLL);
        assert!(RECONNECT_BACKOFF_MAX <= Duration::from_secs(30));
    }

    #[test]
    fn a_message_resets_the_wait_and_cancel_stops() {
        let mut loop_ = ReceiveLoop::new();
        let _ = loop_.on_item(true, true);
        assert_eq!(loop_.on_item(false, true), StreamPoll::Continue);
        assert_eq!(
            loop_.on_item(true, true),
            StreamPoll::Reconnect {
                after: RECONNECT_BACKOFF
            }
        );
        assert_eq!(loop_.on_item(false, false), StreamPoll::Stop);
    }

    #[test]
    fn backoff_caps_at_the_maximum() {
        assert_eq!(
            next_backoff(Duration::from_secs(16)),
            Duration::from_secs(30)
        );
        assert_eq!(
            next_backoff(Duration::from_secs(30)),
            Duration::from_secs(30)
        );
    }
}
