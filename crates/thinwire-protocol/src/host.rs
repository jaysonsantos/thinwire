//! Tokio host: adapters run here; the UI only polls the event channel.

use tokio::runtime::Handle;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use super::{AdapterCommand, AdapterEvent, ProtocolAdapter, registry};

/// Bridge between the UI thread and protocol workers.
pub struct AdapterHost {
    event_rx: UnboundedReceiver<AdapterEvent>,
    command_tx: UnboundedSender<AdapterCommand>,
}

impl AdapterHost {
    /// Spawn the worker that owns every adapter. Safe to call off the UI thread.
    #[must_use]
    pub fn spawn(handle: &Handle) -> Self {
        let (event_tx, event_rx) = unbounded_channel();
        let (command_tx, mut command_rx) = unbounded_channel();

        handle.spawn(async move {
            let mut adapters = registry();
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
        }
    }

    /// Non-blocking poll used by the UI frame. Does not wait on protocol I/O.
    pub fn poll_events(&mut self) -> Vec<AdapterEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.event_rx.try_recv() {
            events.push(event);
        }
        events
    }

    /// Enqueue a command for the worker. Never runs adapter code on the caller.
    pub fn send(&self, command: AdapterCommand) {
        if self.command_tx.send(command).is_err() {
            tracing::warn!("adapter host command channel closed");
        }
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
