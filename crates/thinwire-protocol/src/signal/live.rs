//! Secondary-device client. Compiled only with `signal-local`.
//!
//! Presage runs on a tokio task. The egui thread never calls into this module.
//! Provisioning URLs are redacted events. Message text is not logged.
//! Upstream `presage` logs the provisioning URL at info; the binary filter
//! keeps that target at error.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use presage::Manager;
use presage::libsignal_service::configuration::SignalServers;
use presage::libsignal_service::content::{ContentBody, DataMessage};
use presage::libsignal_service::prelude::{Content, Uuid};
use presage::libsignal_service::protocol::ServiceId;
use presage::manager::Registered;
use presage::model::contacts::Contact;
use presage::model::identity::OnNewIdentity;
use presage::model::messages::Received;
use presage::store::{ContentsStore, StateStore, Thread};
use presage_store_sled::{MigrationConflictStrategy, SledStore};
use tokio::sync::{Mutex, mpsc};

use super::path::{prepare_session_dir, signal_session_path};
use super::reconnect::{ReceiveLoop, StreamPoll};
use crate::adapter::{
    AdapterEvent, AdapterStatus, ChatMessage, Conversation, Delivery, EventTx, ProtocolId,
    RedactedPairingSecret, emit_conversation, emit_message, emit_status,
};

const DATA_DIR_MISSING: &str =
    "Platform app-data directory is unavailable. Signal linking did not start.";
const STORE_FAILED: &str =
    "Signal session store could not be opened under app-data. Nothing was logged.";
const LINK_FAILED: &str = "Signal linking failed. No provisioning URL was logged.";
const SYNC_FAILED: &str = "Signal chat list could not be read. Message text was not logged.";
const SEND_FAILED: &str = "Signal send failed. The message text was not logged.";
const DISCONNECTED: &str = "Signal disconnected. The receive stream ended.";

pub(super) struct Outbound {
    pub(super) conversation_id: String,
    pub(super) body: String,
    pub(super) request: u64,
}

pub(super) struct Session {
    generation: AtomicU64,
    active: AtomicBool,
    outbound: Mutex<Option<mpsc::UnboundedSender<Outbound>>>,
}

impl Session {
    pub(super) fn new() -> Self {
        Self {
            generation: AtomicU64::new(0),
            active: AtomicBool::new(false),
            outbound: Mutex::new(None),
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

    pub(super) async fn submit(&self, message: Outbound) -> bool {
        let guard = self.outbound.lock().await;
        guard.as_ref().is_some_and(|tx| tx.send(message).is_ok())
    }

    pub(super) async fn shutdown(&self) {
        self.active.store(false, Ordering::SeqCst);
        *self.outbound.lock().await = None;
    }
}

pub(super) async fn run(session: Arc<Session>, token: u64, events: EventTx) {
    if !session.is_current(token) {
        session.active.store(false, Ordering::SeqCst);
        return;
    }
    let Ok(path) = signal_session_path() else {
        fail(&events, DATA_DIR_MISSING);
        session.active.store(false, Ordering::SeqCst);
        return;
    };
    let dir = path.clone();
    let prepared = tokio::task::spawn_blocking(move || prepare_session_dir(&dir))
        .await
        .ok()
        .and_then(Result::ok);
    if prepared.is_none() || !session.is_current(token) {
        fail(&events, STORE_FAILED);
        session.active.store(false, Ordering::SeqCst);
        return;
    }
    let Ok(store_path) = signal_session_path() else {
        fail(&events, DATA_DIR_MISSING);
        session.active.store(false, Ordering::SeqCst);
        return;
    };
    let store = match SledStore::open(
        &store_path,
        MigrationConflictStrategy::Drop,
        OnNewIdentity::Trust,
    )
    .await
    {
        Ok(store) => store,
        Err(_) => {
            fail(&events, STORE_FAILED);
            session.active.store(false, Ordering::SeqCst);
            return;
        }
    };
    if !session.is_current(token) {
        session.active.store(false, Ordering::SeqCst);
        return;
    }

    let mut manager = if store.is_registered().await {
        match Manager::load_registered(store).await {
            Ok(manager) => manager,
            Err(_) => {
                fail(&events, LINK_FAILED);
                session.active.store(false, Ordering::SeqCst);
                return;
            }
        }
    } else {
        match link_new(&events, session.as_ref(), token, store).await {
            Ok(manager) => manager,
            Err(()) => {
                fail(&events, LINK_FAILED);
                session.active.store(false, Ordering::SeqCst);
                return;
            }
        }
    };
    if !session.is_current(token) {
        session.active.store(false, Ordering::SeqCst);
        return;
    }
    if publish_chats(&manager, &events).await.is_err() {
        fail(&events, SYNC_FAILED);
        session.active.store(false, Ordering::SeqCst);
        return;
    }
    emit_status(
        &events,
        ProtocolId::Signal,
        AdapterStatus::Ready,
        "Signal is linked on this local build. This binary is not a release.",
    );

    let (tx, mut rx) = mpsc::unbounded_channel();
    *session.outbound.lock().await = Some(tx);
    let mut receive = ReceiveLoop::new();
    while session.is_current(token) {
        let Ok(stream) = manager.receive_messages().await else {
            fail(&events, SYNC_FAILED);
            break;
        };
        let mut pending: Option<Outbound> = None;
        let mut reconnect_after = None;
        {
            let mut incoming = std::pin::pin!(stream);
            loop {
                if !session.is_current(token) {
                    break;
                }
                tokio::select! {
                    biased;
                    command = rx.recv() => {
                        pending = command;
                        break;
                    }
                    item = incoming.next() => {
                        match receive.on_item(item.is_none(), session.is_current(token)) {
                            StreamPoll::Continue => {
                                if let Some(Received::Content(content)) = item {
                                    emit_content(&events, content.as_ref());
                                }
                            }
                            StreamPoll::Reconnect { after } => {
                                reconnect_after = Some(after);
                                break;
                            }
                            StreamPoll::Stop => break,
                        }
                    }
                }
            }
        }
        if let Some(after) = reconnect_after {
            emit_status(
                &events,
                ProtocolId::Signal,
                AdapterStatus::Connecting,
                DISCONNECTED,
            );
            if !wait_backoff(session.as_ref(), token, after).await {
                break;
            }
            continue;
        }
        if let Some(outbound) = pending {
            let request = outbound.request;
            let conversation_id = outbound.conversation_id.clone();
            if send_text(&mut manager, &outbound, &events).await.is_err() {
                crate::adapter::emit_send_rejected(
                    &events,
                    ProtocolId::Signal,
                    conversation_id,
                    request,
                );
                fail(&events, SEND_FAILED);
            }
        }
    }
    session.active.store(false, Ordering::SeqCst);
    *session.outbound.lock().await = None;
}

async fn link_new(
    events: &EventTx,
    session: &Session,
    token: u64,
    store: SledStore,
) -> Result<Manager<SledStore, Registered>, ()> {
    let (prov_tx, prov_rx) = futures::channel::oneshot::channel();
    let linking = Manager::link_secondary_device(
        store,
        SignalServers::Production,
        "thinwire".into(),
        prov_tx,
    );
    let show = async {
        match prov_rx.await {
            Ok(url) if session.is_current(token) => {
                let _ = events.send(AdapterEvent::SignalQr {
                    code: RedactedPairingSecret::new(url.to_string()),
                    generation: token,
                });
                Ok(())
            }
            _ => Err(()),
        }
    };
    let (linked, shown) = tokio::join!(linking, show);
    shown?;
    linked.map_err(|_| ())
}

async fn publish_chats(
    manager: &Manager<SledStore, Registered>,
    events: &EventTx,
) -> Result<(), ()> {
    let contacts = manager.store().contacts().await.map_err(|_| ())?;
    for contact in contacts.flatten() {
        emit_conversation(events, conversation_from_contact(&contact));
        let thread = Thread::Contact(contact.uuid);
        let Ok(messages) = manager.store().messages(&thread, ..).await else {
            continue;
        };
        for message in messages.flatten() {
            emit_content(events, &message);
        }
    }
    Ok(())
}

fn conversation_from_contact(contact: &Contact) -> Conversation {
    let id = contact.uuid.to_string();
    Conversation {
        protocol: ProtocolId::Signal,
        id: id.clone(),
        title: contact.name.clone(),
        participant: id,
        preview: String::new(),
        unread: 0,
        order: i64::from(contact.inbox_position),
        last_at: 0,
        is_group: false,
    }
}

fn emit_content(events: &EventTx, content: &Content) {
    let ContentBody::DataMessage(DataMessage {
        body: Some(body), ..
    }) = &content.body
    else {
        return;
    };
    let Ok(Thread::Contact(uuid)) = Thread::try_from(content) else {
        return;
    };
    let conversation_id = uuid.to_string();
    emit_message(
        events,
        ChatMessage {
            protocol: ProtocolId::Signal,
            conversation_id,
            id: content.metadata.timestamp.to_string(),
            sender: uuid.to_string(),
            body: body.clone(),
            outbound: false,
            delivery: Delivery::Sent,
            sent_at: super::time::sent_at_secs(content.metadata.timestamp),
        },
    );
}

async fn send_text(
    manager: &mut Manager<SledStore, Registered>,
    outbound: &Outbound,
    events: &EventTx,
) -> Result<(), ()> {
    let uuid = Uuid::parse_str(&outbound.conversation_id).map_err(|_| ())?;
    let service_id = ServiceId::Aci(uuid.into());
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ())?
        .as_millis() as u64;
    let data_message = DataMessage {
        body: Some(outbound.body.clone()),
        timestamp: Some(timestamp),
        ..Default::default()
    };
    manager
        .send_message(service_id, data_message, timestamp)
        .await
        .map_err(|_| ())?;
    crate::adapter::emit_send_accepted(
        events,
        ProtocolId::Signal,
        &outbound.conversation_id,
        outbound.request,
    );
    emit_message(
        events,
        ChatMessage {
            protocol: ProtocolId::Signal,
            conversation_id: outbound.conversation_id.clone(),
            id: format!("signal:out:{timestamp}"),
            sender: "me".into(),
            body: outbound.body.clone(),
            outbound: true,
            delivery: Delivery::Sent,
            sent_at: super::time::sent_at_secs(timestamp),
        },
    );
    Ok(())
}

/// `true` when this generation is still current after the wait.
async fn wait_backoff(session: &Session, token: u64, delay: std::time::Duration) -> bool {
    let deadline = tokio::time::Instant::now() + delay;
    while session.is_current(token) {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return true;
        }
        let slice = (deadline - now).min(super::reconnect::RECONNECT_POLL);
        tokio::time::sleep(slice).await;
    }
    false
}

fn fail(events: &EventTx, detail: &str) {
    emit_status(events, ProtocolId::Signal, AdapterStatus::Error, detail);
}
