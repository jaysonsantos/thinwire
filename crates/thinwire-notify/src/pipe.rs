//! The bounded inbox of the notification thread, and its failure policy.
//! Pure logic with no OS call, so the tests run on every platform.

use std::collections::VecDeque;
use std::time::Duration;

use crate::NotifyCommand;

/// Most commands that wait for the backend thread. A slow or hung OS
/// notification service does not grow memory (#87 review).
pub const INBOX_LIMIT: usize = 64;

/// Failed OS calls in a row before notifications turn off for this run.
pub const MAX_FAILURES_IN_A_ROW: u32 = 5;

/// Wait after the first failed OS call. Each failure in a row doubles it,
/// up to `RETRY_MAX`.
pub const RETRY_AFTER: Duration = Duration::from_secs(1);

/// Longest wait between two tries.
pub const RETRY_MAX: Duration = Duration::from_secs(30);

/// Commands that wait for the backend. A new `Show` or `Update` of a chat
/// replaces the queued one of that chat (an `Update` of a queued `Show`
/// stays a `Show`). A `Dismiss` removes the queued text of its chat. Over
/// `INBOX_LIMIT`, the oldest `Show` or `Update` goes; a `Dismiss` never goes.
#[derive(Debug, Default)]
pub(crate) struct Inbox {
    commands: VecDeque<NotifyCommand>,
}

impl Inbox {
    pub(crate) fn push(&mut self, command: NotifyCommand) {
        let key = command.key().clone();
        match &command {
            NotifyCommand::Show(_) => {
                // The newest text wins; a queued `Update` of this chat goes.
                self.commands.retain(
                    |queued| !matches!(queued, NotifyCommand::Update(_) if queued.key() == &key),
                );
                if let Some(slot) = self
                    .commands
                    .iter_mut()
                    .find(|queued| matches!(queued, NotifyCommand::Show(_) if queued.key() == &key))
                {
                    *slot = command;
                    return;
                }
            }
            NotifyCommand::Update(notification) => {
                // Not shown yet: the queued `Show` takes the new text and
                // stays a `Show`.
                if let Some(slot) = self
                    .commands
                    .iter_mut()
                    .find(|queued| queued.is_content() && queued.key() == &key)
                {
                    *slot = match slot {
                        NotifyCommand::Show(_) => NotifyCommand::Show(notification.clone()),
                        _ => command,
                    };
                    return;
                }
            }
            NotifyCommand::Dismiss(_) => {
                self.commands
                    .retain(|queued| !(queued.is_content() && queued.key() == &key));
                if self
                    .commands
                    .iter()
                    .any(|queued| matches!(queued, NotifyCommand::Dismiss(old) if old == &key))
                {
                    return;
                }
            }
        }
        self.commands.push_back(command);
        while self.commands.len() > INBOX_LIMIT {
            let Some(index) = self.commands.iter().position(NotifyCommand::is_content) else {
                break;
            };
            self.commands.remove(index);
        }
    }

    pub(crate) fn pop(&mut self) -> Option<NotifyCommand> {
        self.commands.pop_front()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.commands.len()
    }
}

/// What the thread does after one OS call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Next {
    /// Go on with the next command.
    Go,
    /// Wait this long, then go on. The failed command is not tried again.
    Wait(Duration),
    /// Too many failures in a row: stop for this run.
    Off,
}

/// Failures in a row of the OS backend. One error, for example a failed
/// `CloseNotification` on a busy session bus, does not turn notifications
/// off for the whole run (qa L1).
#[derive(Debug, Default)]
pub(crate) struct Health {
    failures: u32,
    warned: bool,
    off: bool,
}

impl Health {
    #[cfg(test)]
    pub(crate) const fn is_off(&self) -> bool {
        self.off
    }

    pub(crate) fn record(&mut self, result: Result<(), crate::BackendError>) -> Next {
        let Err(crate::BackendError(kind)) = result else {
            self.failures = 0;
            return Next::Go;
        };
        self.failures += 1;
        if self.failures >= MAX_FAILURES_IN_A_ROW {
            self.off = true;
            tracing::warn!(kind, "desktop notifications are off for this run");
            return Next::Off;
        }
        // One warning for the run; later failures log at debug level.
        if self.warned {
            tracing::debug!(
                kind,
                failures = self.failures,
                "desktop notification failed"
            );
        } else {
            self.warned = true;
            tracing::warn!(kind, "desktop notification failed; the app tries again");
        }
        Next::Wait(retry_wait(self.failures))
    }
}

/// Wait after `failures` failures in a row.
pub(crate) fn retry_wait(failures: u32) -> Duration {
    RETRY_AFTER
        .saturating_mul(1 << failures.saturating_sub(1).min(16))
        .min(RETRY_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BackendError, Notification, NotifyKey};
    use thinwire_core::ProtocolId;

    fn key(n: usize) -> NotifyKey {
        NotifyKey {
            protocol: ProtocolId::Telegram,
            conversation_id: format!("telegram:{n}"),
        }
    }

    fn show(n: usize, count: u32) -> NotifyCommand {
        NotifyCommand::Show(Notification {
            key: key(n),
            title: "Ada".into(),
            sender: None,
            preview: "hi".into(),
            count,
        })
    }

    #[test]
    fn the_inbox_coalesces_each_chat_and_keeps_every_dismiss() {
        let mut inbox = Inbox::default();
        inbox.push(show(1, 1));
        inbox.push(show(2, 1));
        inbox.push(show(1, 2));
        assert_eq!(
            inbox.len(),
            2,
            "the newest Show of a chat replaces the queued one"
        );
        assert_eq!(inbox.pop(), Some(show(1, 2)));
        inbox.push(NotifyCommand::Dismiss(key(2)));
        inbox.push(NotifyCommand::Dismiss(key(2)));
        assert_eq!(
            inbox.pop(),
            Some(NotifyCommand::Dismiss(key(2))),
            "the Show left"
        );
        assert!(inbox.is_empty(), "one Dismiss for a chat");

        // A hung backend: many chats wait. Shows are bounded, dismisses stay.
        for n in 0..INBOX_LIMIT * 3 {
            inbox.push(show(n, 1));
        }
        assert_eq!(inbox.len(), INBOX_LIMIT);
        for n in 0..INBOX_LIMIT * 2 {
            inbox.push(NotifyCommand::Dismiss(key(1000 + n)));
        }
        assert_eq!(inbox.len(), INBOX_LIMIT * 2, "every Dismiss stays");
    }

    #[test]
    fn an_update_never_becomes_a_second_show() {
        let update = |n: usize| {
            let NotifyCommand::Show(notification) = show(n, 1) else {
                unreachable!()
            };
            NotifyCommand::Update(Notification {
                preview: "New message".into(),
                ..notification
            })
        };
        let mut inbox = Inbox::default();
        // A queued Show takes the new text and stays a Show.
        inbox.push(show(1, 1));
        inbox.push(update(1));
        assert_eq!(inbox.len(), 1);
        assert!(matches!(inbox.pop(), Some(NotifyCommand::Show(n)) if n.preview == "New message"));
        // Shown already: an Update waits alone; a new Show replaces it.
        inbox.push(update(2));
        inbox.push(update(2));
        assert_eq!(inbox.len(), 1);
        inbox.push(show(2, 2));
        assert!(matches!(inbox.pop(), Some(NotifyCommand::Show(_))));
        assert!(inbox.is_empty());
    }

    #[test]
    fn failures_back_off_and_turn_off_only_after_several_in_a_row() {
        let mut health = Health::default();
        let fail = Err(BackendError("close"));
        assert_eq!(health.record(fail), Next::Wait(RETRY_AFTER));
        assert_eq!(
            health.record(Ok(())),
            Next::Go,
            "a success resets the count"
        );
        for n in 1..MAX_FAILURES_IN_A_ROW {
            assert_eq!(health.record(fail), Next::Wait(retry_wait(n)));
        }
        assert!(!health.is_off());
        assert_eq!(health.record(fail), Next::Off);
        assert!(health.is_off());
        assert_eq!(retry_wait(1), RETRY_AFTER);
        assert_eq!(retry_wait(2), RETRY_AFTER * 2);
        assert_eq!(retry_wait(40), RETRY_MAX);
    }
}
