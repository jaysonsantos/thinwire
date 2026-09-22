//! Linked-device client. Compiled only with `whatsapp-web`.
//!
//! `Bot` runs on a tokio task. The egui thread never calls into this module.
//! QR payloads and pair codes are events, not command fields, and are not logged.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::Mutex;
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
    generation: AtomicU64,
    active: AtomicBool,
    handle: Mutex<Option<whatsapp_rust::bot::BotHandle>>,
}

impl LiveLink {
    pub(super) fn new() -> Self {
        Self {
            generation: AtomicU64::new(0),
            active: AtomicBool::new(false),
            handle: Mutex::new(None),
        }
    }

    pub(super) fn next_generation(&self) -> u64 {
        self.active.store(false, Ordering::SeqCst);
        self.generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub(super) fn mark_active(&self) {
        self.active.store(true, Ordering::SeqCst);
    }

    pub(super) fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    fn is_current(&self, token: u64) -> bool {
        self.generation.load(Ordering::SeqCst) == token
    }

    pub(super) async fn shutdown(&self) {
        self.active.store(false, Ordering::SeqCst);
        if let Some(handle) = self.handle.lock().await.take() {
            handle.shutdown().await;
        }
    }
}

pub(super) async fn run_link(
    link: Arc<LiveLink>,
    token: u64,
    phone: Option<String>,
    events: EventTx,
) {
    if !link.is_current(token) {
        link.active.store(false, Ordering::SeqCst);
        return;
    }
    if let Some(previous) = link.handle.lock().await.take() {
        previous.shutdown().await;
    }
    if !link.is_current(token) {
        link.active.store(false, Ordering::SeqCst);
        return;
    }

    let Ok(path) = whatsapp_device_store_path() else {
        fail(&events, DATA_DIR_MISSING);
        link.active.store(false, Ordering::SeqCst);
        return;
    };
    let Some(parent) = path.parent().map(std::path::Path::to_path_buf) else {
        fail(&events, DATA_DIR_MISSING);
        link.active.store(false, Ordering::SeqCst);
        return;
    };
    let prepared = tokio::task::spawn_blocking(move || prepare_session_dir(&parent))
        .await
        .ok()
        .and_then(Result::ok);
    if prepared.is_none() {
        fail(&events, STORE_FAILED);
        link.active.store(false, Ordering::SeqCst);
        return;
    }
    let Some(db) = path.to_str() else {
        fail(&events, STORE_FAILED);
        link.active.store(false, Ordering::SeqCst);
        return;
    };
    if !link.is_current(token) {
        link.active.store(false, Ordering::SeqCst);
        return;
    }

    let backend = match SqliteStore::new(db).await {
        Ok(backend) => backend,
        Err(_) => {
            fail(&events, STORE_FAILED);
            link.active.store(false, Ordering::SeqCst);
            return;
        }
    };
    let _ = restrict_store_file(&path);
    if !link.is_current(token) {
        link.active.store(false, Ordering::SeqCst);
        return;
    }

    let bot = match build_bot(
        backend,
        digits_only(phone),
        events.clone(),
        Arc::clone(&link),
        token,
    )
    .await
    {
        Ok(bot) => bot,
        Err(()) => {
            fail(&events, BUILD_FAILED);
            link.active.store(false, Ordering::SeqCst);
            return;
        }
    };
    if !link.is_current(token) {
        link.active.store(false, Ordering::SeqCst);
        return;
    }

    let handle = bot.spawn();
    if !link.is_current(token) {
        handle.shutdown().await;
        link.active.store(false, Ordering::SeqCst);
        return;
    }
    *link.handle.lock().await = Some(handle);
    emit_status(
        &events,
        ProtocolId::WhatsApp,
        AdapterStatus::Connecting,
        "Experimental WhatsApp pairing is running on the worker. This is not a supported messenger.",
    );
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
