//! Tokio host: adapters run here; the UI only polls the event channel.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::runtime::Handle;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use super::adapter::LoginEpoch;
use super::{
    AdapterCommand, AdapterEvent, DiscordSecretVault, ProtocolAdapter, ProtocolId,
    TelegramSecretVault, WhatsAppPhoneVault, registry,
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
        whatsapp_phone: Arc<WhatsAppPhoneVault>,
    ) -> Self {
        let (event_tx, event_rx) = unbounded_channel();
        let (command_tx, mut command_rx) = unbounded_channel();
        let login_epoch: LoginEpoch = Arc::new(AtomicU64::new(0));
        let adapter_epoch = Arc::clone(&login_epoch);

        handle.spawn(async move {
            let mut adapters = registry(secrets, discord, whatsapp_phone, adapter_epoch);
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

    /// Enqueue a command for the worker. Never runs adapter code on the caller.
    pub fn send(&self, mut command: AdapterCommand) {
        // Stamp first, then bump. A step sent before Cancel keeps the old
        // epoch; the adapter ignores it instead of emitting `Failed` after
        // the secrets were cleared.
        let epoch = self.login_epoch.load(Ordering::SeqCst);
        stamp_auth_epoch(&mut command, epoch);
        // Bump here, on the UI thread, before the next poll: login events that
        // an old client already queued are then stale and dropped (issue #42).
        if ends_telegram_login(&command) {
            self.login_epoch.fetch_add(1, Ordering::SeqCst);
        }
        if self.command_tx.send(command).is_err() {
            tracing::warn!("adapter host command channel closed");
        }
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

fn dispatch(
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
    if let Err(error) = adapter.handle(command, events) {
        tracing::info!(%error, "adapter refused or failed a command");
        let _ = events.send(AdapterEvent::Status {
            protocol: error_protocol(&error),
            status: match &error {
                super::AdapterError::Refused { .. } => super::AdapterStatus::Refused,
                super::AdapterError::Unavailable { .. } => super::AdapterStatus::Error,
            },
            detail: error.to_string(),
        });
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

    #[tokio::test]
    async fn the_host_bumps_the_epoch_on_the_ui_thread_when_it_sends_cancel() {
        let vault = Arc::new(crate::MemorySecretVault::new());
        let host = AdapterHost::spawn(
            &Handle::current(),
            Arc::clone(&vault) as Arc<dyn TelegramSecretVault>,
            Arc::new(crate::MemoryDiscordVault::new()) as Arc<dyn DiscordSecretVault>,
            Arc::new(WhatsAppPhoneVault::new()),
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
}
