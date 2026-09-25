//! Tokio host: adapters run here; the UI only polls the event channel.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::runtime::Handle;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use super::adapter::LoginEpoch;
use super::{
    AdapterCommand, AdapterEvent, DiscordSecretVault, ProtocolAdapter, ProtocolId,
    SlackSecretVault, TelegramSecretVault, WhatsAppPhoneVault, registry,
};

/// Bridge between the UI thread and protocol workers.
pub struct AdapterHost {
    event_rx: UnboundedReceiver<AdapterEvent>,
    command_tx: UnboundedSender<AdapterCommand>,
    /// Telegram login epoch, shared with the Telegram adapter.
    login_epoch: LoginEpoch,
}

impl AdapterHost {
    /// Spawn the worker that owns every adapter. Safe to call off the UI thread.
    #[must_use]
    pub fn spawn(
        handle: &Handle,
        secrets: Arc<dyn TelegramSecretVault>,
        discord: Arc<dyn DiscordSecretVault>,
        slack: Arc<dyn SlackSecretVault>,
        whatsapp_phone: Arc<WhatsAppPhoneVault>,
        signal: Option<Box<dyn ProtocolAdapter>>,
    ) -> Self {
        let login_epoch: LoginEpoch = Arc::new(AtomicU64::new(0));
        let adapter_epoch = Arc::clone(&login_epoch);
        Self::spawn_inner(handle, login_epoch, move || {
            registry(
                secrets,
                discord,
                slack,
                whatsapp_phone,
                adapter_epoch,
                signal,
            )
        })
    }

    /// Spawn the worker with these adapters in place of the app's registry.
    /// For the demo adapters (#120) and tests. The routing, the login epoch,
    /// and the error handling are the same as in [`Self::spawn`].
    #[must_use]
    pub fn spawn_adapters(handle: &Handle, adapters: Vec<Box<dyn ProtocolAdapter>>) -> Self {
        Self::spawn_inner(handle, Arc::new(AtomicU64::new(0)), move || adapters)
    }

    fn spawn_inner(
        handle: &Handle,
        login_epoch: LoginEpoch,
        adapters: impl FnOnce() -> Vec<Box<dyn ProtocolAdapter>> + Send + 'static,
    ) -> Self {
        let (event_tx, event_rx) = unbounded_channel();
        let (command_tx, mut command_rx) = unbounded_channel();

        handle.spawn(async move {
            let mut adapters = adapters();
            for adapter in &mut adapters {
                adapter.start(event_tx.clone());
            }

            while let Some(command) = command_rx.recv().await {
                dispatch(&mut adapters, command, &event_tx);
            }
        });

        Self {
            event_rx,
            command_tx,
            login_epoch,
        }
    }

    /// Non-blocking poll used by the UI frame. Does not wait on protocol I/O.
    pub fn poll_events(&mut self) -> Vec<AdapterEvent> {
        let current = self.login_epoch.load(Ordering::SeqCst);
        let mut events = Vec::new();
        while let Ok(event) = self.event_rx.try_recv() {
            if let Some(event) = deliver(event, current) {
                events.push(event);
            }
        }
        events
    }

    /// Clone of the worker command channel.
    ///
    /// Keychain hydration uses this to reconnect Discord after `spawn_blocking`
    /// finishes. Sending does not run adapter code on the caller.
    #[must_use]
    pub fn command_sender(&self) -> UnboundedSender<AdapterCommand> {
        self.command_tx.clone()
    }

    /// Split into a [`HostSender`] and the raw event receiver.
    ///
    /// A caller that waits on events (a frontend core) owns the receiver on
    /// its own task. It must pass each event through [`HostSender::deliver`]
    /// on the thread that sends commands, so the Telegram login epoch rules
    /// of `send` and `poll_events` still hold (issue #42).
    #[must_use]
    pub fn into_parts(self) -> (HostSender, UnboundedReceiver<AdapterEvent>) {
        (
            HostSender {
                command_tx: self.command_tx,
                login_epoch: self.login_epoch,
            },
            self.event_rx,
        )
    }

    /// Enqueue a command for the worker. Never runs adapter code on the caller.
    pub fn send(&self, command: AdapterCommand) {
        if !send_stamped(&self.command_tx, &self.login_epoch, command) {
            tracing::warn!("adapter host command channel closed");
        }
    }
}

/// Command side of a split [`AdapterHost`]. It keeps the Telegram login
/// epoch rules: `send` stamps and bumps like [`AdapterHost::send`], and
/// `deliver` unwraps and drops like [`AdapterHost::poll_events`].
#[derive(Debug, Clone)]
pub struct HostSender {
    command_tx: UnboundedSender<AdapterCommand>,
    login_epoch: LoginEpoch,
}

impl HostSender {
    /// Enqueue a command for the worker. False when the worker is gone.
    pub fn send(&self, command: AdapterCommand) -> bool {
        send_stamped(&self.command_tx, &self.login_epoch, command)
    }

    /// Unwrap a stamped login event with the epoch of now; `None` for a stale
    /// one. Call it on the thread that calls `send`, just before the event is
    /// applied, so a Cancel sent before this call drops the event.
    #[must_use]
    pub fn deliver(&self, event: AdapterEvent) -> Option<AdapterEvent> {
        deliver(event, self.login_epoch.load(Ordering::SeqCst))
    }

    /// Test hook: a sender over a channel the test reads, with its own epoch.
    #[doc(hidden)]
    #[must_use]
    pub fn for_test(command_tx: UnboundedSender<AdapterCommand>) -> Self {
        Self {
            command_tx,
            login_epoch: Arc::new(AtomicU64::new(0)),
        }
    }
}

/// Stamp first, then bump. A step sent before Cancel keeps the old epoch; the
/// adapter ignores it instead of emitting `Failed` after the secrets were
/// cleared. The bump happens on the sending thread, before the next delivery:
/// login events that an old client already queued are then stale (issue #42).
fn send_stamped(
    command_tx: &UnboundedSender<AdapterCommand>,
    login_epoch: &LoginEpoch,
    mut command: AdapterCommand,
) -> bool {
    let epoch = login_epoch.load(Ordering::SeqCst);
    stamp_auth_epoch(&mut command, epoch);
    if ends_telegram_login(&command) {
        login_epoch.fetch_add(1, Ordering::SeqCst);
    }
    command_tx.send(command).is_ok()
}

/// The error status of a failed login step carries the step's epoch, so a
/// Cancel that happens after the step started still drops it (PR #49 review).
fn stamp_login_failure(auth_epoch: Option<u64>, status: AdapterEvent) -> AdapterEvent {
    match auth_epoch {
        Some(epoch) => AdapterEvent::Login {
            epoch,
            event: Box::new(status),
        },
        None => status,
    }
}

/// Record the login epoch a Telegram step was sent under.
fn stamp_auth_epoch(command: &mut AdapterCommand, epoch: u64) {
    if let AdapterCommand::TelegramAuth { epoch: slot, .. } = command {
        *slot = epoch;
    }
}

/// Commands that end the current Telegram client's login flow.
fn ends_telegram_login(command: &AdapterCommand) -> bool {
    matches!(
        command,
        AdapterCommand::Disconnect {
            protocol: ProtocolId::Telegram
        } | AdapterCommand::Shutdown {
            protocol: ProtocolId::Telegram
        }
    )
}

/// Unwrap a stamped login event if its epoch is current; drop a stale one.
/// Other events pass unchanged.
fn deliver(event: AdapterEvent, current_epoch: u64) -> Option<AdapterEvent> {
    match event {
        AdapterEvent::Login { epoch, event } => (epoch == current_epoch).then_some(*event),
        other => Some(other),
    }
}

pub(crate) fn dispatch(
    adapters: &mut [Box<dyn ProtocolAdapter>],
    command: AdapterCommand,
    events: &super::EventTx,
) {
    let protocol = command.protocol();
    let Some(adapter) = adapters.iter_mut().find(|adapter| adapter.id() == protocol) else {
        tracing::warn!(%protocol, "no adapter registered");
        return;
    };
    // Every adapter answers Shutdown, so the app can wait for all of them.
    if matches!(command, AdapterCommand::Shutdown { .. }) {
        adapter.shutdown(events);
        return;
    }
    // The viewed chat is a hint to the adapter, not a request: route it to
    // the trait method, so an adapter with no read state needs no arm.
    if let AdapterCommand::ViewChat {
        conversation_id, ..
    } = &command
    {
        adapter.view_chat(conversation_id.as_deref(), events);
        return;
    }
    let auth_epoch = match command {
        AdapterCommand::TelegramAuth { epoch, .. } => Some(epoch),
        _ => None,
    };
    if let Err(error) = adapter.handle(command, events) {
        tracing::info!(%error, "adapter refused or failed a command");
        let status = AdapterEvent::Status {
            protocol: error_protocol(&error),
            status: match &error {
                super::AdapterError::Refused { .. } => super::AdapterStatus::Refused,
                super::AdapterError::Unavailable { .. } => super::AdapterStatus::Error,
            },
            detail: error.to_string(),
        };
        let _ = events.send(stamp_login_failure(auth_epoch, status));
    }
}

fn error_protocol(error: &super::AdapterError) -> super::ProtocolId {
    match *error {
        super::AdapterError::Refused { protocol, .. }
        | super::AdapterError::Unavailable { protocol, .. } => protocol,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TelegramAuthPhase;

    fn stamped(epoch: u64, phase: TelegramAuthPhase) -> AdapterEvent {
        AdapterEvent::Login {
            epoch,
            event: Box::new(AdapterEvent::TelegramAuth { phase }),
        }
    }

    #[test]
    fn an_auth_step_is_stamped_with_the_epoch_it_was_sent_under() {
        use crate::TelegramAuthStep;
        let mut step = AdapterCommand::TelegramAuth {
            step: TelegramAuthStep::Phone,
            epoch: 99,
        };
        stamp_auth_epoch(&mut step, 0);
        assert!(matches!(
            step,
            AdapterCommand::TelegramAuth {
                step: TelegramAuthStep::Phone,
                epoch: 0
            }
        ));
    }

    #[test]
    fn a_login_event_queued_before_cancel_is_dropped_at_delivery() {
        // A fake old client queued NeedPhone and Ready under epoch 0. Then the
        // user pressed Cancel: the host sent Disconnect and bumped to 1.
        let cancel = AdapterCommand::Disconnect {
            protocol: ProtocolId::Telegram,
        };
        assert!(ends_telegram_login(&cancel));
        let current = 1;
        assert_eq!(
            deliver(stamped(0, TelegramAuthPhase::NeedPhone), current),
            None
        );
        assert_eq!(deliver(stamped(0, TelegramAuthPhase::Ready), current), None);
        // The new client's events (epoch 1) arrive unwrapped.
        assert_eq!(
            deliver(stamped(1, TelegramAuthPhase::NeedPhone), current),
            Some(AdapterEvent::TelegramAuth {
                phase: TelegramAuthPhase::NeedPhone
            })
        );
        // Other events are never stamped and always pass.
        let reset = AdapterEvent::TelegramDataReset {
            moved_to: "tdlib.stale-1".into(),
        };
        assert_eq!(deliver(reset.clone(), current), Some(reset));
        assert!(!ends_telegram_login(&AdapterCommand::LoadChats {
            protocol: ProtocolId::Telegram
        }));
        assert!(!ends_telegram_login(&AdapterCommand::Disconnect {
            protocol: ProtocolId::WhatsApp
        }));
    }

    #[test]
    fn a_failed_step_status_is_stamped_and_dropped_after_cancel() {
        let status = AdapterEvent::Status {
            protocol: ProtocolId::Telegram,
            status: crate::AdapterStatus::Error,
            detail: "telegram phone is missing from the secret store".into(),
        };
        let stamped = stamp_login_failure(Some(0), status.clone());
        // Cancel bumped the epoch to 1 before the UI polled.
        assert_eq!(
            deliver(stamped.clone(), 1),
            None,
            "no failure on a cancelled flow"
        );
        assert_eq!(
            deliver(stamped, 0),
            Some(status.clone()),
            "shown when still current"
        );
        assert_eq!(
            stamp_login_failure(None, status.clone()),
            status,
            "other commands unchanged"
        );
    }

    #[tokio::test]
    async fn the_host_bumps_the_epoch_on_the_ui_thread_when_it_sends_cancel() {
        let vault = Arc::new(crate::MemorySecretVault::new());
        let host = AdapterHost::spawn(
            &Handle::current(),
            Arc::clone(&vault) as Arc<dyn TelegramSecretVault>,
            Arc::new(crate::MemoryDiscordVault::new()) as Arc<dyn DiscordSecretVault>,
            Arc::new(crate::MemorySlackVault::new()) as Arc<dyn SlackSecretVault>,
            Arc::new(WhatsAppPhoneVault::new()),
            None,
        );
        assert_eq!(host.login_epoch.load(Ordering::SeqCst), 0);
        host.send(AdapterCommand::Disconnect {
            protocol: ProtocolId::Telegram,
        });
        assert_eq!(
            host.login_epoch.load(Ordering::SeqCst),
            1,
            "bumped at send time, before the worker handles the command"
        );
    }

    #[test]
    fn a_split_host_sender_keeps_the_login_epoch_rules() {
        let (tx, mut rx) = unbounded_channel();
        let sender = HostSender::for_test(tx);
        let queued_before_cancel = stamped(0, TelegramAuthPhase::Ready);
        assert!(sender.send(AdapterCommand::Disconnect {
            protocol: ProtocolId::Telegram,
        }));
        assert_eq!(sender.deliver(queued_before_cancel), None);
        assert_eq!(
            sender.deliver(stamped(1, TelegramAuthPhase::NeedPhone)),
            Some(AdapterEvent::TelegramAuth {
                phase: TelegramAuthPhase::NeedPhone
            })
        );
        // A step sent now carries the new epoch.
        assert!(sender.send(AdapterCommand::TelegramAuth {
            step: crate::TelegramAuthStep::Phone,
            epoch: 99,
        }));
        let _disconnect = rx.try_recv().expect("disconnect");
        assert!(matches!(
            rx.try_recv().expect("step"),
            AdapterCommand::TelegramAuth { epoch: 1, .. }
        ));
    }

    #[test]
    fn view_chat_reaches_the_default_method_and_emits_nothing() {
        let mut adapters: Vec<Box<dyn ProtocolAdapter>> =
            vec![Box::new(crate::FakeAdapter::default())];
        let (tx, mut rx) = unbounded_channel();
        for conversation_id in [Some("telegram:1".to_owned()), None] {
            dispatch(
                &mut adapters,
                AdapterCommand::ViewChat {
                    protocol: ProtocolId::Telegram,
                    conversation_id,
                },
                &tx,
            );
        }
        assert!(
            rx.try_recv().is_err(),
            "no status, no error for a viewed-chat hint"
        );
    }
}
