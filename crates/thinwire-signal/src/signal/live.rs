// SPDX-License-Identifier: AGPL-3.0-only
//! Secondary-device client. Compiled only with `signal-local`.
//!
//! Presage runs on a tokio task. The egui thread never calls into this module.
//! Provisioning URLs are redacted events. Message text is not logged.
//! Upstream `presage` logs the provisioning URL at info; the binary filter
//! keeps that target at error.

use std::collections::{HashMap, HashSet};
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
use presage::model::groups::Group;
use presage::model::identity::OnNewIdentity;
use presage::model::messages::Received;
use presage::store::{ContentsStore, StateStore, Thread};
use presage_store_sqlite::SqliteStore;
use tokio::sync::{Mutex, mpsc};

use super::path::{
    prepare_session_dir, signal_session_path, sled_session_present, sqlite_store_path,
};
use super::reconnect::{ReceiveLoop, Relink, StreamPoll};
use super::wake::AttemptCancel;
use thinwire_protocol::{
    AccountState, AdapterEvent, AdapterStatus, ChatMessage, Conversation, Delivery, EventTx,
    ProtocolId, RedactedPairingSecret, emit_account, emit_conversation, emit_message, emit_status,
};

const DATA_DIR_MISSING: &str =
    "Platform app-data directory is unavailable. Signal linking did not start.";
const STORE_FAILED: &str =
    "Signal session store could not be opened under app-data. Nothing was logged.";
const SLED_RELINK: &str = "This build stores the Signal session in SQLite. A session saved by the previous sled store cannot be opened. Link this device again. The old files stay on disk.";
const LINK_FAILED: &str = "Signal linking failed. No provisioning URL was logged.";
const SYNC_FAILED: &str = "Signal chat list could not be read. Message text was not logged.";
const SEND_FAILED: &str = "Signal send failed. The message text was not logged.";
const DISCONNECTED: &str = "Signal disconnected. The receive stream ended.";

pub(super) struct Outbound {
    pub(super) conversation_id: String,
    pub(super) body: String,
    pub(super) request: u64,
}

/// One page of stored history. The same size as the Telegram older page.
const HISTORY_PAGE: usize = 50;

pub(super) enum WorkerJob {
    Send(Outbound),
    Older {
        conversation_id: String,
        before_message_id: String,
    },
}

pub(super) struct Session {
    generation: AtomicU64,
    active: AtomicBool,
    outbound: Mutex<Option<mpsc::UnboundedSender<WorkerJob>>>,
    cancel: std::sync::Mutex<AttemptCancel>,
    pairing: AtomicU64,
    sent: Mutex<std::collections::HashMap<String, String>>,
    groups: Mutex<super::group::GroupKeys>,
}

impl Session {
    pub(super) fn new() -> Self {
        Self {
            generation: AtomicU64::new(0),
            active: AtomicBool::new(false),
            outbound: Mutex::new(None),
            cancel: std::sync::Mutex::new(AttemptCancel::new()),
            pairing: AtomicU64::new(0),
            sent: Mutex::new(std::collections::HashMap::new()),
            groups: Mutex::new(super::group::GroupKeys::default()),
        }
    }

    pub(super) fn set_pairing(&self, generation: u64) {
        self.pairing.store(generation, Ordering::SeqCst);
    }

    fn pairing(&self) -> u64 {
        self.pairing.load(Ordering::SeqCst)
    }

    pub(super) async fn remember(&self, message_id: &str, body: &str) {
        self.sent
            .lock()
            .await
            .insert(message_id.to_string(), body.to_string());
    }

    pub(super) async fn recall(&self, message_id: &str) -> Option<String> {
        self.sent.lock().await.get(message_id).cloned()
    }

    pub(super) async fn remember_group(&self, master_key: &[u8]) -> String {
        self.groups.lock().await.remember(master_key)
    }

    pub(super) async fn group_key(&self, id: &str) -> Option<[u8; 32]> {
        self.groups.lock().await.key(id)
    }

    fn bump_generation(&self) -> u64 {
        self.active.store(false, Ordering::SeqCst);
        self.generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Install a new cancel token and return it with this attempt's generation.
    pub(super) fn begin_attempt(&self) -> (u64, Arc<tokio::sync::Notify>) {
        let attempt = AttemptCancel::new();
        let wake = attempt.handle();
        *self.cancel.lock().expect("cancel") = attempt;
        let token = self.bump_generation();
        (token, wake)
    }

    /// Cancel only the token of the current attempt.
    pub(super) fn cancel_attempt(&self) -> u64 {
        let next = self.bump_generation();
        self.cancel.lock().expect("cancel").cancel();
        next
    }

    pub(super) fn mark_active(&self) {
        self.active.store(true, Ordering::SeqCst);
    }

    pub(super) fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    pub(super) fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    fn is_current(&self, token: u64) -> bool {
        self.generation.load(Ordering::SeqCst) == token
    }

    pub(super) async fn submit(&self, message: Outbound) -> bool {
        self.enqueue(WorkerJob::Send(message)).await
    }

    pub(super) async fn request_older(
        &self,
        conversation_id: String,
        before_message_id: String,
    ) -> bool {
        self.enqueue(WorkerJob::Older {
            conversation_id,
            before_message_id,
        })
        .await
    }

    async fn enqueue(&self, job: WorkerJob) -> bool {
        let guard = self.outbound.lock().await;
        guard.as_ref().is_some_and(|tx| tx.send(job).is_ok())
    }

    pub(super) async fn shutdown(&self) {
        self.active.store(false, Ordering::SeqCst);
        *self.outbound.lock().await = None;
    }
}

pub(super) async fn run(
    session: Arc<Session>,
    token: u64,
    wake: Arc<tokio::sync::Notify>,
    events: EventTx,
) {
    run_linked(Arc::clone(&session), token, wake, events.clone()).await;
    if session.is_current(token) {
        emit_account(&events, ProtocolId::Signal, AccountState::Unlinked);
    }
}

async fn run_linked(
    session: Arc<Session>,
    token: u64,
    wake: Arc<tokio::sync::Notify>,
    events: EventTx,
) {
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
    let Some(store_url) = sqlite_store_path(&store_path).to_str().map(str::to_string) else {
        fail(&events, STORE_FAILED);
        session.active.store(false, Ordering::SeqCst);
        return;
    };
    let store = match SqliteStore::open(&store_url, OnNewIdentity::Trust).await {
        Ok(store) => store,
        Err(_) => {
            fail(&events, STORE_FAILED);
            session.active.store(false, Ordering::SeqCst);
            return;
        }
    };
    if sled_session_present(&store_path) && !store.is_registered().await {
        emit_status(
            &events,
            ProtocolId::Signal,
            AdapterStatus::Connecting,
            SLED_RELINK,
        );
    }
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
    emit_account(&events, ProtocolId::Signal, AccountState::Linked);
    let (mut known, mut group_titles) =
        match publish_chats(&manager, session.as_ref(), &events).await {
            Ok(published) => published,
            Err(()) => {
                fail(&events, SYNC_FAILED);
                session.active.store(false, Ordering::SeqCst);
                return;
            }
        };
    let mut names = contact_names(&manager).await;
    emit_status(
        &events,
        ProtocolId::Signal,
        AdapterStatus::Ready,
        "Signal is linked on this local build. This binary is not a release.",
    );

    let (tx, mut rx) = mpsc::unbounded_channel();
    *session.outbound.lock().await = Some(tx);
    let mut receive = ReceiveLoop::new();
    let mut relink = Relink::new();
    while session.is_current(token) {
        let Ok(stream) = manager.receive_messages().await else {
            if let StreamPoll::Reconnect { after } =
                receive.on_open_failed(session.is_current(token))
            {
                emit_account(&events, ProtocolId::Signal, AccountState::Linking);
                emit_status(
                    &events,
                    ProtocolId::Signal,
                    AdapterStatus::Connecting,
                    DISCONNECTED,
                );
                if !wait_backoff(session.as_ref(), token, after).await {
                    break;
                }
                relink.note_end();
                continue;
            }
            break;
        };
        if relink.on_stream() {
            names = contact_names(&manager).await;
            emit_account(&events, ProtocolId::Signal, AccountState::Linked);
            emit_status(
                &events,
                ProtocolId::Signal,
                AdapterStatus::Ready,
                "Signal is linked on this local build. This binary is not a release.",
            );
        }
        let mut pending: Option<WorkerJob> = None;
        let mut reconnect_after = None;
        {
            let mut incoming = std::pin::pin!(stream);
            loop {
                if !session.is_current(token) {
                    break;
                }
                tokio::select! {
                    biased;
                    _ = wake.notified() => {
                        break;
                    }
                    command = rx.recv() => {
                        pending = command;
                        break;
                    }
                    item = incoming.next() => {
                        match receive.on_item(item.is_none(), session.is_current(token)) {
                            StreamPoll::Continue => {
                                if let Some(Received::Content(content)) = item {
                                    if let Ok(Thread::Group(key)) = Thread::try_from(content.as_ref())
                                    {
                                        session.remember_group(&key).await;
                                    }
                                    remember_group_title(
                                        &manager,
                                        content.as_ref(),
                                        &mut group_titles,
                                    )
                                    .await;
                                    emit_incoming(
                                        &events,
                                        content.as_ref(),
                                        &names,
                                        &group_titles,
                                        &mut known,
                                    );
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
            emit_account(&events, ProtocolId::Signal, AccountState::Linking);
            emit_status(
                &events,
                ProtocolId::Signal,
                AdapterStatus::Connecting,
                DISCONNECTED,
            );
            if !wait_backoff(session.as_ref(), token, after).await {
                break;
            }
            relink.note_end();
            continue;
        }
        match pending {
            Some(WorkerJob::Send(outbound)) => {
                let request = outbound.request;
                let conversation_id = outbound.conversation_id.clone();
                if send_text(&mut manager, &session, &outbound, &events)
                    .await
                    .is_err()
                {
                    thinwire_protocol::emit_send_rejected(
                        &events,
                        ProtocolId::Signal,
                        conversation_id,
                        request,
                    );
                    fail(&events, SEND_FAILED);
                }
            }
            Some(WorkerJob::Older {
                conversation_id,
                before_message_id,
            }) => {
                load_older_page(
                    &manager,
                    session.as_ref(),
                    &events,
                    &names,
                    &group_titles,
                    &conversation_id,
                    &before_message_id,
                )
                .await;
            }
            None => {}
        }
    }
    session.active.store(false, Ordering::SeqCst);
    *session.outbound.lock().await = None;
}

async fn link_new(
    events: &EventTx,
    session: &Session,
    token: u64,
    store: SqliteStore,
) -> Result<Manager<SqliteStore, Registered>, ()> {
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
                    generation: session.pairing(),
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

async fn contact_names(manager: &Manager<SqliteStore, Registered>) -> HashMap<String, String> {
    let mut names = HashMap::new();
    let Ok(contacts) = manager.store().contacts().await else {
        return names;
    };
    for contact in contacts.flatten() {
        if !contact.name.is_empty() {
            names.insert(contact.uuid.to_string(), contact.name);
        }
    }
    names
}

async fn publish_chats(
    manager: &Manager<SqliteStore, Registered>,
    session: &Session,
    events: &EventTx,
) -> Result<(HashSet<String>, HashMap<String, String>), ()> {
    let mut known = HashSet::new();
    let mut group_titles = HashMap::new();
    let names = contact_names(manager).await;
    let contacts = manager.store().contacts().await.map_err(|_| ())?;
    for contact in contacts.flatten() {
        let conversation = conversation_from_contact(&contact);
        known.insert(conversation.id.clone());
        emit_conversation(events, conversation);
        let thread = Thread::Contact(ServiceId::Aci(contact.uuid.into()));
        let Ok(messages) = manager.store().messages(&thread, ..).await else {
            continue;
        };
        for message in last_page(messages.flatten(), HISTORY_PAGE) {
            emit_content(events, &message, &names, &group_titles);
        }
    }
    let groups = manager.store().groups().await.map_err(|_| ())?;
    for group in groups.flatten() {
        let (key, group) = group;
        let conversation = conversation_from_group(&key, &group);
        session.remember_group(&key).await;
        group_titles.insert(conversation.id.clone(), conversation.title.clone());
        known.insert(conversation.id.clone());
        emit_conversation(events, conversation);
        let thread = Thread::Group(key);
        let Ok(messages) = manager.store().messages(&thread, ..).await else {
            continue;
        };
        for message in last_page(messages.flatten(), HISTORY_PAGE) {
            emit_content(events, &message, &names, &group_titles);
        }
    }
    Ok((known, group_titles))
}

/// Keep the newest `page` items. `more` is true when older items were dropped.
fn last_page<T>(items: impl IntoIterator<Item = T>, page: usize) -> Vec<T> {
    let mut kept = std::collections::VecDeque::new();
    for item in items {
        if page == 0 {
            return Vec::new();
        }
        if kept.len() == page {
            kept.pop_front();
        }
        kept.push_back(item);
    }
    kept.into_iter().collect()
}

async fn load_older_page(
    manager: &Manager<SqliteStore, Registered>,
    session: &Session,
    events: &EventTx,
    names: &HashMap<String, String>,
    group_titles: &HashMap<String, String>,
    conversation_id: &str,
    before_message_id: &str,
) {
    let loaded = async {
        let before = before_message_id.parse::<u64>().ok()?;
        let thread = thread_of(conversation_id, session.group_key(conversation_id).await)?;
        let messages = manager.store().messages(&thread, ..before).await.ok()?;
        Some(last_page_more(messages.flatten(), HISTORY_PAGE))
    }
    .await;
    let (page, more) = loaded.unwrap_or_else(|| (Vec::new(), false));
    for message in page {
        emit_content(events, &message, names, group_titles);
    }
    let _ = events.send(AdapterEvent::OlderHistoryLoaded {
        protocol: ProtocolId::Signal,
        conversation_id: conversation_id.to_string(),
        before_message_id: before_message_id.to_string(),
        more,
        note: None,
    });
}

fn thread_of(conversation_id: &str, group_key: Option<[u8; 32]>) -> Option<Thread> {
    if super::group::is_group_id(conversation_id) {
        return group_key.map(Thread::Group);
    }
    Uuid::parse_str(conversation_id)
        .ok()
        .map(|uuid| Thread::Contact(ServiceId::Aci(uuid.into())))
}

fn sent_sync_body(sync: &presage::libsignal_service::proto::SyncMessage) -> Option<&str> {
    use presage::libsignal_service::proto::sync_message::Content as SyncContent;
    match sync.content.as_ref()? {
        SyncContent::Sent(sent) => sent.message.as_ref()?.body.as_deref(),
        _ => None,
    }
}

fn last_page_more<T>(items: impl IntoIterator<Item = T>, page: usize) -> (Vec<T>, bool) {
    let mut kept = std::collections::VecDeque::new();
    let mut more = false;
    for item in items {
        if page == 0 {
            return (Vec::new(), true);
        }
        if kept.len() == page {
            kept.pop_front();
            more = true;
        }
        kept.push_back(item);
    }
    (kept.into_iter().collect(), more)
}

async fn remember_group_title(
    manager: &Manager<SqliteStore, Registered>,
    content: &Content,
    titles: &mut HashMap<String, String>,
) {
    let Ok(Thread::Group(key)) = Thread::try_from(content) else {
        return;
    };
    let id = super::group::group_id(&key);
    if titles.contains_key(&id) {
        return;
    }
    let stored = manager
        .store()
        .group(key)
        .await
        .ok()
        .flatten()
        .map(|group| group.title);
    let title = super::group::group_chat(&key, stored.as_deref().unwrap_or("")).title;
    titles.insert(id, title);
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
        writable: true,
        placeholder: false,
    }
}

fn conversation_from_group(key: &[u8], group: &Group) -> Conversation {
    let chat = super::group::group_chat(key, &group.title);
    Conversation {
        protocol: ProtocolId::Signal,
        id: chat.id.clone(),
        title: chat.title,
        participant: chat.id,
        preview: String::new(),
        unread: 0,
        order: 0,
        last_at: 0,
        is_group: chat.is_group,
        writable: true,
        placeholder: false,
    }
}

fn emit_incoming(
    events: &EventTx,
    content: &Content,
    names: &HashMap<String, String>,
    group_titles: &HashMap<String, String>,
    known: &mut HashSet<String>,
) {
    let Some((conversation, message)) = row_and_message(content, names, group_titles) else {
        return;
    };
    known.insert(conversation.id.clone());
    emit_conversation(events, conversation);
    emit_message(events, message);
}

fn emit_content(
    events: &EventTx,
    content: &Content,
    names: &HashMap<String, String>,
    group_titles: &HashMap<String, String>,
) {
    let Some((conversation, message)) = row_and_message(content, names, group_titles) else {
        return;
    };
    emit_conversation(events, conversation);
    emit_message(events, message);
}

fn row_and_message(
    content: &Content,
    names: &HashMap<String, String>,
    group_titles: &HashMap<String, String>,
) -> Option<(Conversation, ChatMessage)> {
    let incoming = match &content.body {
        ContentBody::DataMessage(DataMessage { body, .. }) => {
            super::message::Incoming::Data(body.as_deref())
        }
        ContentBody::SynchronizeMessage(sync) => {
            super::message::Incoming::SentSync(sent_sync_body(sync))
        }
        _ => super::message::Incoming::Other,
    };
    let shown = super::message::visible_text(incoming)?;
    let thread = Thread::try_from(content).ok()?;
    let uuid = content.metadata.sender.raw_uuid().to_string();
    let (conversation_id, sender, title, is_group) = match &thread {
        Thread::Contact(id) => {
            let conversation_id = id.raw_uuid().to_string();
            let title = names
                .get(&conversation_id)
                .filter(|name| !name.is_empty())
                .cloned()
                .unwrap_or_else(|| conversation_id.clone());
            (conversation_id, uuid, title, false)
        }
        Thread::Group(key) => {
            let conversation_id = super::group::group_id(key);
            let title = group_titles
                .get(&conversation_id)
                .cloned()
                .unwrap_or_else(|| super::group::group_chat(key, "").title);
            (
                conversation_id,
                super::group::sender_name(&uuid, names),
                title,
                true,
            )
        }
    };
    let sent_millis =
        u64::try_from(content.metadata.client_timestamp.timestamp_millis()).unwrap_or(0);
    let message = ChatMessage {
        protocol: ProtocolId::Signal,
        conversation_id: conversation_id.clone(),
        id: sent_millis.to_string(),
        sender: if shown.outbound {
            "me".to_string()
        } else {
            sender
        },
        body: shown.body.to_string(),
        outbound: shown.outbound,
        delivery: Delivery::Sent,
        sent_at: super::time::sent_at_secs(sent_millis),
    };
    let conversation = Conversation {
        protocol: ProtocolId::Signal,
        id: conversation_id.clone(),
        title,
        participant: conversation_id,
        preview: message.body.clone(),
        unread: u32::from(!message.outbound),
        order: message.sent_at,
        last_at: message.sent_at,
        is_group,
        writable: true,
        placeholder: false,
    };
    Some((conversation, message))
}

async fn send_text(
    manager: &mut Manager<SqliteStore, Registered>,
    session: &Session,
    outbound: &Outbound,
    events: &EventTx,
) -> Result<(), ()> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ())?
        .as_millis() as u64;
    let data_message = DataMessage {
        body: Some(outbound.body.clone()),
        timestamp: Some(timestamp),
        ..Default::default()
    };
    if super::group::is_group_id(&outbound.conversation_id) {
        let master_key = session
            .group_key(&outbound.conversation_id)
            .await
            .ok_or(())?;
        manager
            .send_message_to_group(&master_key, data_message, timestamp)
            .await
            .map_err(|_| ())?;
    } else {
        let uuid = Uuid::parse_str(&outbound.conversation_id).map_err(|_| ())?;
        let service_id = ServiceId::Aci(uuid.into());
        manager
            .send_message(service_id, data_message, timestamp)
            .await
            .map_err(|_| ())?;
    }
    thinwire_protocol::emit_send_accepted(
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
    let id = format!("signal:out:{timestamp}");
    session.remember(&id, &outbound.body).await;
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

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::sync::Arc;

    use presage::libsignal_service::content::{Content, GroupContextV2, Metadata};
    use presage::libsignal_service::prelude::Uuid;
    use presage::libsignal_service::protocol::DeviceId;
    use presage::libsignal_service::protocol::ServiceId;

    use super::*;

    fn envelope(sender: Uuid, body: DataMessage) -> Content {
        Content::from_body(
            body,
            Metadata {
                sender: ServiceId::Aci(sender.into()),
                destination: ServiceId::Aci(Uuid::nil().into()),
                sender_device: DeviceId::new(1).expect("device"),
                pni_verified: None,
                client_timestamp: chrono::DateTime::from_timestamp_millis(1_700_000_000_000)
                    .expect("time"),
                server_timestamp: chrono::DateTime::from_timestamp_millis(1_700_000_000_000)
                    .expect("time"),
                needs_receipt: false,
                unidentified_sender: false,
                was_plaintext: false,
                server_guid: None,
            },
        )
    }

    fn events_for(content: &Content, names: &HashMap<String, String>) -> Vec<AdapterEvent> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut known = HashSet::new();
        emit_incoming(&tx, content, names, &HashMap::new(), &mut known);
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    #[tokio::test]
    async fn a_retry_after_cancel_keeps_running() {
        let session = Session::new();
        let (first, first_wake) = session.begin_attempt();
        session.cancel_attempt();
        assert!(!session.is_current(first));
        let (retry, retry_wake) = session.begin_attempt();
        first_wake.notify_one();
        let pending =
            tokio::time::timeout(std::time::Duration::from_millis(50), retry_wake.notified()).await;
        assert!(pending.is_err(), "the retry must not see the old cancel");
        assert!(session.is_current(retry));
    }

    #[tokio::test]
    async fn cancel_during_start_stops_only_that_attempt() {
        let session = Session::new();
        let (first, first_wake) = session.begin_attempt();
        let waiting = Arc::clone(&first_wake);
        let handle = tokio::spawn(async move {
            waiting.notified().await;
        });
        session.cancel_attempt();
        tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("that attempt stops")
            .expect("task");
        assert!(!session.is_current(first));
        let (second, second_wake) = session.begin_attempt();
        let pending =
            tokio::time::timeout(std::time::Duration::from_millis(50), second_wake.notified())
                .await;
        assert!(pending.is_err(), "the next attempt stays up");
        assert!(session.is_current(second));
    }

    #[test]
    fn an_unknown_direct_thread_gets_a_row_before_the_message() {
        let contact = Uuid::from_u128(0x1111_1111_1111_1111_1111_1111_1111_1111);
        let content = envelope(
            contact,
            DataMessage {
                body: Some("hello from a new thread".into()),
                ..Default::default()
            },
        );
        let mut names = HashMap::new();
        names.insert(contact.to_string(), "Ada".into());
        let events = events_for(&content, &names);
        assert!(matches!(
            events.first(),
            Some(AdapterEvent::ConversationUpsert { conversation })
                if conversation.id == contact.to_string()
                    && conversation.title == "Ada"
                    && !conversation.is_group
                    && !conversation.placeholder
        ));
        assert!(matches!(
            events.get(1),
            Some(AdapterEvent::MessageReceived { message })
                if message.conversation_id == contact.to_string()
                    && message.body == "hello from a new thread"
        ));
    }

    #[test]
    fn an_unknown_group_thread_gets_a_row_before_the_message() {
        let sender = Uuid::from_u128(0x2222_2222_2222_2222_2222_2222_2222_2222);
        let key = vec![0x11; 32];
        let content = envelope(
            sender,
            DataMessage {
                body: Some("in the group".into()),
                group_v2: Some(GroupContextV2 {
                    master_key: Some(key.clone()),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        let events = events_for(&content, &HashMap::new());
        let id = super::super::group::group_id(&key);
        assert!(matches!(
            events.first(),
            Some(AdapterEvent::ConversationUpsert { conversation })
                if conversation.id == id && conversation.is_group && !conversation.placeholder
        ));
        assert!(matches!(
            events.get(1),
            Some(AdapterEvent::MessageReceived { message })
                if message.conversation_id == id && message.body == "in the group"
        ));
    }

    #[test]
    fn a_known_thread_refreshes_the_row_before_the_message() {
        let contact = Uuid::from_u128(0x1111_1111_1111_1111_1111_1111_1111_1111);
        let content = envelope(
            contact,
            DataMessage {
                body: Some("second line".into()),
                ..Default::default()
            },
        );
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut known = HashSet::new();
        known.insert(contact.to_string());
        emit_incoming(&tx, &content, &HashMap::new(), &HashMap::new(), &mut known);
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        assert!(matches!(
            events.first(),
            Some(AdapterEvent::ConversationUpsert { conversation })
                if conversation.id == contact.to_string()
                    && conversation.preview == "second line"
                    && conversation.last_at == conversation.order
                    && conversation.order > 0
        ));
        assert!(matches!(
            events.get(1),
            Some(AdapterEvent::MessageReceived { message })
                if message.body == "second line"
        ));
    }

    #[test]
    fn a_history_page_keeps_only_the_newest_messages() {
        let (page, more) = last_page_more(1..=60, 50);
        assert!(more);
        assert_eq!(page.first().copied(), Some(11));
        assert_eq!(page.last().copied(), Some(60));
        let (short, more) = last_page_more(1..=10, 50);
        assert!(!more);
        assert_eq!(short.len(), 10);
    }

    #[test]
    fn a_stored_group_title_stays_on_a_new_message() {
        let sender = Uuid::from_u128(0x3333_3333_3333_3333_3333_3333_3333_3333);
        let key = vec![0x22; 32];
        let content = envelope(
            sender,
            DataMessage {
                body: Some("named group".into()),
                group_v2: Some(GroupContextV2 {
                    master_key: Some(key.clone()),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        let id = super::super::group::group_id(&key);
        let mut titles = HashMap::new();
        titles.insert(id.clone(), "Book club".into());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut known = HashSet::new();
        known.insert(id.clone());
        emit_incoming(&tx, &content, &HashMap::new(), &titles, &mut known);
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        assert!(matches!(
            events.first(),
            Some(AdapterEvent::ConversationUpsert { conversation })
                if conversation.id == id && conversation.title == "Book club"
        ));
    }
}
