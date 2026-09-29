//! A backend for an OS service that knows each notification by a tag:
//! Windows toasts and the macOS notification center (#161). The rules here
//! have no OS code, so Linux CI tests them.

use std::hash::{DefaultHasher, Hash, Hasher};

use crate::{Backend, BackendError, Notification, NotifyKey};

/// Tag of the notification of `key`: one per chat, so a new message
/// replaces it. It is a hash: Windows allows at most 64 characters, and the
/// chat id does not go to the OS store. It stays the same only for one run;
/// a start removes the notifications of an earlier run.
pub(crate) fn tag(key: &NotifyKey) -> String {
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// One OS notification service with tags.
pub(crate) trait TagService: Send + 'static {
    /// Show `notification` under `tag`. It replaces a shown notification
    /// with the same tag. `quiet`: no popup and no sound.
    fn post(
        &mut self,
        tag: &str,
        notification: &Notification,
        quiet: bool,
    ) -> Result<(), BackendError>;

    /// The OS still has a notification with `tag`, on the screen or in its
    /// notification list.
    fn has(&mut self, tag: &str) -> Result<bool, BackendError>;

    /// Remove the notification with `tag` from the OS.
    fn remove(&mut self, tag: &str) -> Result<(), BackendError>;

    /// Drop what the service keeps for `tag`, for example its click handler.
    fn forget(&mut self, _tag: &str) {}
}

/// `Backend` for a `TagService`.
pub(crate) struct Tagged<S>(pub(crate) S);

impl<S: TagService> Backend for Tagged<S> {
    fn show(&mut self, notification: &Notification) -> Result<(), BackendError> {
        self.0.post(&tag(&notification.key), notification, false)
    }

    /// Replace only a notification that the OS still has. Else show
    /// nothing (#160 review).
    fn update(&mut self, notification: &Notification) -> Result<(), BackendError> {
        let tag = tag(&notification.key);
        if self.0.has(&tag)? {
            self.0.post(&tag, notification, true)
        } else {
            self.0.forget(&tag);
            Ok(())
        }
    }

    /// A failed remove keeps the click handler: a later dismiss tries again.
    fn dismiss(&mut self, key: &NotifyKey) -> Result<(), BackendError> {
        let tag = tag(key);
        if self.0.has(&tag)? {
            self.0.remove(&tag)?;
        }
        self.0.forget(&tag);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use thinwire_core::ProtocolId;

    #[derive(Debug, PartialEq, Eq)]
    enum Call {
        Post(String, bool),
        Remove(String),
        Forget(String),
    }

    /// Fake OS: `listed` holds the tags that the OS still has.
    #[derive(Default)]
    struct FakeOs {
        calls: Vec<Call>,
        listed: HashSet<String>,
        fail_has: bool,
        fail_remove: bool,
    }

    impl TagService for FakeOs {
        fn post(
            &mut self,
            tag: &str,
            _notification: &Notification,
            quiet: bool,
        ) -> Result<(), BackendError> {
            self.calls.push(Call::Post(tag.into(), quiet));
            self.listed.insert(tag.into());
            Ok(())
        }

        fn has(&mut self, tag: &str) -> Result<bool, BackendError> {
            if self.fail_has {
                return Err(BackendError("history"));
            }
            Ok(self.listed.contains(tag))
        }

        fn remove(&mut self, tag: &str) -> Result<(), BackendError> {
            if self.fail_remove {
                return Err(BackendError("remove"));
            }
            self.calls.push(Call::Remove(tag.into()));
            self.listed.remove(tag);
            Ok(())
        }

        fn forget(&mut self, tag: &str) {
            self.calls.push(Call::Forget(tag.into()));
        }
    }

    fn key(protocol: ProtocolId, id: &str) -> NotifyKey {
        NotifyKey {
            protocol,
            conversation_id: id.into(),
        }
    }

    fn note(id: &str, count: u32) -> Notification {
        Notification {
            key: key(ProtocolId::Telegram, id),
            title: "Ada".into(),
            sender: None,
            preview: "hi".into(),
            count,
        }
    }

    #[test]
    fn a_tag_is_short_and_one_per_chat() {
        let chat = key(ProtocolId::Telegram, "telegram:12345");
        assert_eq!(tag(&chat), tag(&chat.clone()));
        assert_ne!(tag(&chat), tag(&key(ProtocolId::Telegram, "telegram:1")));
        assert_ne!(
            tag(&key(ProtocolId::Slack, "same")),
            tag(&key(ProtocolId::Discord, "same"))
        );
        let long = key(ProtocolId::Slack, &"x".repeat(500));
        assert!(tag(&long).len() <= 64);
        assert!(!tag(&chat).contains("12345"), "no chat id in the OS store");
    }

    #[test]
    fn a_new_message_replaces_the_notification_of_its_chat() {
        let mut backend = Tagged(FakeOs::default());
        backend.show(&note("telegram:1", 1)).expect("show");
        backend.show(&note("telegram:1", 2)).expect("show");
        let chat = tag(&note("telegram:1", 1).key);
        assert_eq!(
            backend.0.calls,
            vec![Call::Post(chat.clone(), false), Call::Post(chat, false)]
        );
    }

    #[test]
    fn an_update_changes_only_a_listed_notification_and_quietly() {
        let mut backend = Tagged(FakeOs::default());
        let chat = tag(&note("telegram:1", 1).key);
        // Not shown: no new notification (#160 review).
        backend.update(&note("telegram:1", 1)).expect("update");
        assert_eq!(backend.0.calls, vec![Call::Forget(chat.clone())]);
        backend.0.calls.clear();

        backend.show(&note("telegram:1", 1)).expect("show");
        backend.update(&note("telegram:1", 1)).expect("update");
        assert_eq!(
            backend.0.calls,
            vec![Call::Post(chat.clone(), false), Call::Post(chat, true)]
        );
    }

    #[test]
    fn a_dismiss_removes_a_listed_notification_and_forgets_it() {
        let mut backend = Tagged(FakeOs::default());
        let chat = tag(&note("telegram:1", 1).key);
        backend.show(&note("telegram:1", 1)).expect("show");
        backend
            .dismiss(&note("telegram:1", 1).key)
            .expect("dismiss");
        assert_eq!(
            backend.0.calls,
            vec![
                Call::Post(chat.clone(), false),
                Call::Remove(chat.clone()),
                Call::Forget(chat.clone()),
            ]
        );
        // The user closed it already: no OS remove, the handler still goes.
        backend.0.calls.clear();
        backend
            .dismiss(&note("telegram:1", 1).key)
            .expect("dismiss");
        assert_eq!(backend.0.calls, vec![Call::Forget(chat)]);
    }

    #[test]
    fn a_failed_call_keeps_the_click_handler() {
        let mut backend = Tagged(FakeOs::default());
        backend.show(&note("telegram:1", 1)).expect("show");
        backend.0.calls.clear();
        backend.0.fail_remove = true;
        assert!(backend.dismiss(&note("telegram:1", 1).key).is_err());
        backend.0.fail_remove = false;
        backend.0.fail_has = true;
        assert!(backend.dismiss(&note("telegram:1", 1).key).is_err());
        assert!(backend.update(&note("telegram:1", 1)).is_err());
        assert_eq!(backend.0.calls, vec![], "no forget, no post");
    }
}
