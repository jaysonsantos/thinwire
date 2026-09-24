//! Linked-device client. Compiled only with `whatsapp-web`.
//!
//! `Bot` runs on a tokio task. The egui thread never calls into this module.
//! QR payloads and pair codes are events, not command fields, and are not logged.
//!
//! Registration and shutdown share [`super::gate::LinkGate`]. A close that
//! lands after `spawn` and before `publish` waits. It does not report the
//! link idle and then let this task install the bot.

use std::sync::Arc;
use std::time::Duration;

use whatsapp_rust::bot::Bot;
use whatsapp_rust::pair_code::PairCodeOptions;
use whatsapp_rust::store::SqliteStore;

use super::path::{prepare_session_dir, restrict_store_file, whatsapp_device_store_path};
use crate::adapter::{
    AdapterEvent, AdapterStatus, EventTx, ProtocolId, RedactedPairingSecret, emit_status,
};

const DATA_DIR_MISSING: &str =
    "Platform app-data directory is unavailable. WhatsApp pairing did not start.";
const STORE_FAILED: &str =
    "WhatsApp device store could not be opened under app-data. Nothing was logged.";
const BUILD_FAILED: &str =
    "WhatsApp pairing client could not be built. No session material was logged.";

pub(super) struct LiveLink {
    gate: super::gate::LinkGate<whatsapp_rust::bot::BotHandle>,
}

impl LiveLink {
    pub(super) fn new() -> Self {
        Self {
            gate: super::gate::LinkGate::new(),
        }
    }

    pub(super) fn next_generation(&self) -> u64 {
        self.gate.next_generation()
    }

    pub(super) fn mark_active(&self) {
        self.gate.mark_active();
    }

    pub(super) fn is_active(&self) -> bool {
        self.gate.is_active()
    }

    fn is_current(&self, token: u64) -> bool {
        self.gate.is_current(token)
    }

    /// Close the bot from an older generation, if one is still stored.
    ///
    /// Starts newer than the closing generation stay installed.
    pub(super) async fn shutdown(&self) {
        if let Some(handle) = self.gate.shutdown().await {
            handle.shutdown().await;
        }
    }
}

enum Session {
    /// Bot is stored. The flight is already over.
    Started,
    /// Bot was spawned after this generation closed. Still in flight until
    /// the caller shuts it down and leaves.
    Rejected(whatsapp_rust::bot::BotHandle),
    /// No bot. Still in flight until the caller leaves.
    Aborted,
}

pub(super) async fn run_link(
    link: Arc<LiveLink>,
    token: u64,
    phone: Option<String>,
    events: EventTx,
) {
    if !link.gate.enter(token).await {
        link.gate.set_inactive();
        return;
    }
    match session(&link, token, phone, events).await {
        Session::Started => {}
        Session::Rejected(handle) => {
            handle.shutdown().await;
            link.gate.leave(token).await;
            link.gate.set_inactive();
        }
        Session::Aborted => {
            link.gate.leave(token).await;
            link.gate.set_inactive();
        }
    }
}

async fn session(
    link: &Arc<LiveLink>,
    token: u64,
    phone: Option<String>,
    events: EventTx,
) -> Session {
    if let Some(previous) = link.gate.take_older(token).await {
        previous.shutdown().await;
    }
    if !link.is_current(token) {
        return Session::Aborted;
    }

    let Ok(path) = whatsapp_device_store_path() else {
        fail(&events, DATA_DIR_MISSING);
        return Session::Aborted;
    };
    let Some(parent) = path.parent().map(std::path::Path::to_path_buf) else {
        fail(&events, DATA_DIR_MISSING);
        return Session::Aborted;
    };
    let prepared = tokio::task::spawn_blocking(move || prepare_session_dir(&parent))
        .await
        .ok()
        .and_then(Result::ok);
    if prepared.is_none() {
        fail(&events, STORE_FAILED);
        return Session::Aborted;
    }
    let Some(db) = path.to_str() else {
        fail(&events, STORE_FAILED);
        return Session::Aborted;
    };
    if !link.is_current(token) {
        return Session::Aborted;
    }

    let backend = match SqliteStore::new(db).await {
        Ok(backend) => backend,
        Err(_) => {
            fail(&events, STORE_FAILED);
            return Session::Aborted;
        }
    };
    let _ = restrict_store_file(&path);
    if !link.is_current(token) {
        return Session::Aborted;
    }

    let bot = match build_bot(
        backend,
        digits_only(phone),
        events.clone(),
        Arc::clone(link),
        token,
    )
    .await
    {
        Ok(bot) => bot,
        Err(()) => {
            fail(&events, BUILD_FAILED);
            return Session::Aborted;
        }
    };

    let handle = bot.spawn();
    match link.gate.publish(token, handle).await {
        Ok(previous) => {
            if let Some(previous) = previous {
                previous.shutdown().await;
            }
            if link.is_current(token) {
                emit_status(
                    &events,
                    ProtocolId::WhatsApp,
                    AdapterStatus::Connecting,
                    "Experimental WhatsApp pairing is running on the worker. This is not a supported messenger.",
                );
            }
            Session::Started
        }
        Err(handle) => Session::Rejected(handle),
    }
}

fn fail(events: &EventTx, detail: &str) {
    emit_status(events, ProtocolId::WhatsApp, AdapterStatus::Error, detail);
}

fn digits_only(phone: Option<String>) -> Option<String> {
    let raw = phone?;
    let digits: String = raw.chars().filter(|ch| ch.is_ascii_digit()).collect();
    if digits.is_empty() {
        None
    } else {
        Some(digits)
    }
}

fn emit_if_current(link: &LiveLink, token: u64, events: &EventTx, event: AdapterEvent) {
    if !link.is_current(token) {
        return;
    }
    let _ = events.send(event);
}

async fn build_bot(
    backend: SqliteStore,
    phone_number: Option<String>,
    events: EventTx,
    link: Arc<LiveLink>,
    token: u64,
) -> Result<Bot, ()> {
    if let Some(phone_number) = phone_number {
        let events_qr = events.clone();
        let events_pair = events;
        let link_qr = Arc::clone(&link);
        let link_pair = link;
        Bot::builder()
            .with_backend(backend)
            .on_qr_code(move |code, _timeout: Duration| {
                let events_qr = events_qr.clone();
                let link_qr = Arc::clone(&link_qr);
                async move {
                    emit_if_current(
                        &link_qr,
                        token,
                        &events_qr,
                        AdapterEvent::WhatsAppQr {
                            code: RedactedPairingSecret::new(code),
                            generation: token,
                        },
                    );
                }
            })
            .with_pair_code(PairCodeOptions {
                phone_number,
                ..Default::default()
            })
            .on_pair_code(move |code, _timeout: Duration| {
                let events_pair = events_pair.clone();
                let link_pair = Arc::clone(&link_pair);
                async move {
                    emit_if_current(
                        &link_pair,
                        token,
                        &events_pair,
                        AdapterEvent::WhatsAppPairCode {
                            code: RedactedPairingSecret::new(code),
                            generation: token,
                        },
                    );
                }
            })
            .build()
            .await
            .map_err(|_| ())
    } else {
        Bot::builder()
            .with_backend(backend)
            .on_qr_code(move |code, _timeout: Duration| {
                let events = events.clone();
                let link = Arc::clone(&link);
                async move {
                    emit_if_current(
                        &link,
                        token,
                        &events,
                        AdapterEvent::WhatsAppQr {
                            code: RedactedPairingSecret::new(code),
                            generation: token,
                        },
                    );
                }
            })
            .build()
            .await
            .map_err(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::unbounded_channel;

    #[test]
    fn stale_pairing_events_are_discarded() {
        let link = LiveLink::new();
        let stale = link.next_generation();
        let current = link.next_generation();
        let (tx, mut rx) = unbounded_channel();

        emit_if_current(
            &link,
            stale,
            &tx,
            AdapterEvent::WhatsAppQr {
                code: RedactedPairingSecret::new("old-qr"),
                generation: stale,
            },
        );
        emit_if_current(
            &link,
            stale,
            &tx,
            AdapterEvent::WhatsAppPairCode {
                code: RedactedPairingSecret::new("old-pair"),
                generation: stale,
            },
        );
        assert!(rx.try_recv().is_err());

        emit_if_current(
            &link,
            current,
            &tx,
            AdapterEvent::WhatsAppQr {
                code: RedactedPairingSecret::new("new-qr"),
                generation: current,
            },
        );
        match rx.try_recv() {
            Ok(AdapterEvent::WhatsAppQr { generation, code }) => {
                assert_eq!(generation, current);
                assert_eq!(code.reveal(), "new-qr");
            }
            other => panic!("expected current qr, got {other:?}"),
        }
        emit_if_current(
            &link,
            current,
            &tx,
            AdapterEvent::WhatsAppPairCode {
                code: RedactedPairingSecret::new("new-pair"),
                generation: current,
            },
        );
        match rx.try_recv() {
            Ok(AdapterEvent::WhatsAppPairCode { generation, code }) => {
                assert_eq!(generation, current);
                assert_eq!(code.reveal(), "new-pair");
            }
            other => panic!("expected current pair code, got {other:?}"),
        }
    }
}
