//! Helper side of the pipe: runs one adapter behind the wire protocol.
//!
//! A helper program calls [`serve`] with its adapter, its stdin, and its
//! stdout. The loop routes each request the way the app's host does, and it
//! sends every adapter event back as a line.

use std::sync::Arc;
use std::time::Duration;

use thinwire_ipc::{
    AppLine, FrameError, FrameReader, HelperLine, HelperRefusal, PROTOCOL_VERSION, WireCommand,
    WireProtocol, write_line,
};
use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use super::convert::{command_from_wire, event_to_wire, wire_protocol};
use super::supervisor::ADAPTER_STOP_WAIT;
use crate::adapter::{
    AdapterCommand, AdapterError, AdapterEvent, AdapterStatus, EventTx, ProtocolAdapter, ProtocolId,
};
use crate::whatsapp::WhatsAppPhoneVault;

/// Start-up input of [`serve`].
#[derive(Debug, Clone)]
pub struct ServeConfig {
    /// The version of the helper program, for `Hello`.
    pub helper_version: &'static str,
    /// The vault that the adapter reads the pair-code phone from. The phone
    /// of a pairing start goes there, never on an adapter command.
    pub phone: Option<Arc<WhatsAppPhoneVault>>,
}

/// Why [`serve`] returned. In each case the adapter shut down first.
#[derive(Debug)]
pub enum ServeEnd {
    /// The app sent `Shutdown`.
    Shutdown,
    /// The app closed the pipe. It exited, or it stopped this helper.
    AppGone,
    /// The app sent a line that is not in the protocol (ADR 0012 section 4).
    BadLine(FrameError),
    /// The adapter's protocol does not run in a helper.
    NotAHelperProtocol,
}

/// Tell the app that this helper cannot run, in place of `Hello`.
pub async fn refuse<W: AsyncWrite + Unpin>(writer: &mut W, reason: HelperRefusal) {
    let _ = write_line(writer, &HelperLine::Refused { reason }).await;
}

/// Run `adapter` behind the wire protocol until the app ends the session.
pub async fn serve<R, W>(
    mut adapter: Box<dyn ProtocolAdapter>,
    config: ServeConfig,
    reader: R,
    mut writer: W,
) -> ServeEnd
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let protocol = adapter.id();
    let Some(wire) = wire_protocol(protocol) else {
        return ServeEnd::NotAHelperProtocol;
    };
    let hello = HelperLine::Hello {
        protocol_version: PROTOCOL_VERSION,
        helper_version: config.helper_version.to_owned(),
        protocols: vec![wire],
    };
    if write_line(&mut writer, &hello).await.is_err() {
        return ServeEnd::AppGone;
    }
    let (events, mut event_rx) = unbounded_channel();
    adapter.start(events.clone());
    let mut frames = FrameReader::new(BufReader::new(reader));
    let mut pipe_open = true;
    let end = loop {
        tokio::select! {
            line = frames.next::<AppLine>() => match line {
                Ok(Some(AppLine::Request { id, protocol: named, command })) => {
                    if named != wire {
                        break ServeEnd::BadLine(FrameError::Unexpected);
                    }
                    tracing::debug!(id, command = command.kind(), "helper request");
                    route(adapter.as_mut(), protocol, command, &config, &events);
                    if write_line(&mut writer, &HelperLine::Ack { id }).await.is_err() {
                        pipe_open = false;
                        break ServeEnd::AppGone;
                    }
                }
                Ok(Some(AppLine::Shutdown)) => break ServeEnd::Shutdown,
                Ok(None) => break ServeEnd::AppGone,
                Err(error) => break ServeEnd::BadLine(error),
            },
            Some(event) = event_rx.recv() => {
                if !forward(&mut writer, protocol, wire, event).await {
                    pipe_open = false;
                    break ServeEnd::AppGone;
                }
            }
        }
    };
    stop(
        adapter.as_mut(),
        protocol,
        wire,
        &events,
        &mut event_rx,
        pipe_open.then_some(&mut writer),
    )
    .await;
    end
}

/// Route one request like the app's host: the viewed chat goes to
/// `view_chat`, and an error from `handle` becomes a `Status` event.
fn route(
    adapter: &mut dyn ProtocolAdapter,
    protocol: ProtocolId,
    command: WireCommand,
    config: &ServeConfig,
    events: &EventTx,
) {
    let begins = matches!(command, WireCommand::BeginLink { .. });
    let Some((command, phone)) = command_from_wire(protocol, command) else {
        report(
            events,
            &AdapterError::Unavailable {
                protocol,
                reason: "this helper does not have that command",
            },
        );
        return;
    };
    if begins && let Some(vault) = &config.phone {
        // A pairing with no phone must not use the number of an older one.
        match &phone {
            Some(phone) => vault.set_phone(phone.expose()),
            None => vault.clear(),
        }
    }
    if let AdapterCommand::ViewChat {
        conversation_id, ..
    } = &command
    {
        adapter.view_chat(conversation_id.as_deref(), events);
        return;
    }
    if let Err(error) = adapter.handle(command, events) {
        report(events, &error);
    }
}

fn report(events: &EventTx, error: &AdapterError) {
    let (protocol, status) = match *error {
        AdapterError::Refused { protocol, .. } => (protocol, AdapterStatus::Refused),
        AdapterError::Unavailable { protocol, .. } => (protocol, AdapterStatus::Error),
    };
    let _ = events.send(AdapterEvent::Status {
        protocol,
        status,
        detail: error.to_string(),
    });
}

/// Write one adapter event. False when the pipe is closed.
async fn forward<W: AsyncWrite + Unpin>(
    writer: &mut W,
    protocol: ProtocolId,
    wire: WireProtocol,
    event: AdapterEvent,
) -> bool {
    let Some(event) = event_to_wire(protocol, event) else {
        return true;
    };
    let line = HelperLine::Event {
        protocol: wire,
        event,
    };
    match write_line(writer, &line).await {
        Ok(()) => true,
        // One event that is too long is dropped. The pipe stays open.
        Err(FrameError::TooLong) => {
            tracing::warn!("helper event dropped: the line is too long");
            true
        }
        Err(_) => false,
    }
}

/// Close every session of the adapter. `Stopped` goes to the app when the
/// adapter confirms it in time and the pipe is open.
async fn stop<W: AsyncWrite + Unpin>(
    adapter: &mut dyn ProtocolAdapter,
    protocol: ProtocolId,
    wire: WireProtocol,
    events: &EventTx,
    event_rx: &mut UnboundedReceiver<AdapterEvent>,
    mut writer: Option<&mut W>,
) {
    adapter.shutdown(events);
    let stopped = wait_stopped(protocol, wire, event_rx, &mut writer, ADAPTER_STOP_WAIT).await;
    if stopped && let Some(writer) = writer {
        let _ = write_line(writer, &HelperLine::Stopped { protocol: wire }).await;
    }
}

/// Send the last events until the adapter says `Stopped`. False when `wait`
/// ran out first: a client can still run, so the helper does not say
/// `Stopped`. The app then ends the process.
async fn wait_stopped<W: AsyncWrite + Unpin>(
    protocol: ProtocolId,
    wire: WireProtocol,
    event_rx: &mut UnboundedReceiver<AdapterEvent>,
    writer: &mut Option<&mut W>,
    wait: Duration,
) -> bool {
    let wait = tokio::time::sleep(wait);
    tokio::pin!(wait);
    loop {
        tokio::select! {
            () = &mut wait => return false,
            event = event_rx.recv() => match event {
                Some(AdapterEvent::Stopped { protocol: seen }) if seen == protocol => return true,
                Some(event) => {
                    if let Some(open) = writer.as_deref_mut()
                        && !forward(open, protocol, wire, event).await
                    {
                        *writer = None;
                    }
                }
                None => return false,
            },
        }
    }
}
