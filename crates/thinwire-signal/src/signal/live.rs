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
/// Rows one store read may return. One extra row is the older-page cursor.
/// One more tells the page that older rows exist. The store primary key is
/// `(ts, thread_id)`, so a time window can still hold more rows than a page.
const HISTORY_READ_LIMIT: usize = HISTORY_PAGE + 2;

pub(super) enum WorkerJob {
    Send(Outbound),
    Older {
        conversation_id: String,
        before_message_id: String,
    },
    /// Reread contacts and groups. `LoadChats` sends this.
    Refresh,
    /// The user opened or left a chat. The id is the chat at enqueue time.
    /// Clear that chat's unread count.
    Viewed(Option<String>),
}

pub(super) struct Session {
    generation: AtomicU64,
    active: AtomicBool,
    outbound: std::sync::Mutex<Option<mpsc::UnboundedSender<WorkerJob>>>,
    cancel: std::sync::Mutex<AttemptCancel>,
    pairing: AtomicU64,
    sent: Mutex<std::collections::HashMap<String, String>>,
    groups: Mutex<super::group::GroupKeys>,
    viewed: std::sync::Mutex<Option<String>>,
}

impl Session {
    pub(super) fn new() -> Self {
        Self {
            generation: AtomicU64::new(0),
            active: AtomicBool::new(false),
            outbound: std::sync::Mutex::new(None),
            cancel: std::sync::Mutex::new(AttemptCancel::new()),
            pairing: AtomicU64::new(0),
            sent: Mutex::new(std::collections::HashMap::new()),
            groups: Mutex::new(super::group::GroupKeys::default()),
            viewed: std::sync::Mutex::new(None),
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

    pub(super) fn set_viewed(&self, conversation_id: Option<String>) {
        *self.viewed.lock().expect("viewed") = conversation_id;
    }

    fn viewed(&self) -> Option<String> {
        self.viewed.lock().expect("viewed").clone()
    }

    pub(super) fn submit(&self, message: Outbound) -> bool {
        self.enqueue(WorkerJob::Send(message))
    }

    pub(super) fn request_older(&self, conversation_id: String, before_message_id: String) -> bool {
        self.enqueue(WorkerJob::Older {
            conversation_id,
            before_message_id,
        })
    }

    pub(super) fn request_refresh(&self) -> bool {
        self.enqueue(WorkerJob::Refresh)
    }

    /// Queue a clear for the chat just opened or left.
    /// The job stores that id for the worker.
    pub(super) fn request_viewed(&self, conversation_id: Option<String>) -> bool {
        self.enqueue(WorkerJob::Viewed(conversation_id))
    }

    #[cfg(test)]
    pub(super) fn install_jobs_for_test(&self) -> mpsc::UnboundedReceiver<WorkerJob> {
        let (tx, rx) = mpsc::unbounded_channel();
        *self.outbound.lock().expect("outbound") = Some(tx);
        rx
    }

    fn enqueue(&self, job: WorkerJob) -> bool {
        self.outbound
            .lock()
            .expect("outbound")
            .as_ref()
            .is_some_and(|tx| tx.send(job).is_ok())
    }

    pub(super) fn shutdown(&self) {
        self.active.store(false, Ordering::SeqCst);
        *self.outbound.lock().expect("outbound") = None;
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
    let mut rows = std::collections::HashMap::<String, Conversation>::new();
    let mut unread = std::collections::HashMap::<String, u32>::new();
    let (mut known, mut group_titles) =
        match publish_chats(&manager, session.as_ref(), &events, &mut rows, &unread).await {
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
    *session.outbound.lock().expect("outbound") = Some(tx);
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
                                        &mut unread,
                                        session.viewed().as_deref(),
                                        &mut rows,
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
                if send_text(
                    &mut manager,
                    &session,
                    &outbound,
                    &events,
                    &names,
                    &group_titles,
                    &mut rows,
                    &mut unread,
                )
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
                    &mut rows,
                )
                .await;
            }
            Some(WorkerJob::Refresh) => {
                match publish_chats(&manager, session.as_ref(), &events, &mut rows, &unread).await {
                    Ok((next_known, next_titles)) => {
                        known = next_known;
                        group_titles = next_titles;
                        names = contact_names(&manager).await;
                    }
                    Err(()) => fail(&events, SYNC_FAILED),
                }
                thinwire_protocol::emit_chat_list_loaded(&events, ProtocolId::Signal);
            }
            Some(WorkerJob::Viewed(conversation_id)) => {
                clear_viewed(&events, &mut unread, &mut rows, conversation_id);
            }
            None => {}
        }
    }
    session.active.store(false, Ordering::SeqCst);
    *session.outbound.lock().expect("outbound") = None;
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
    rows: &mut HashMap<String, Conversation>,
    unread: &HashMap<String, u32>,
) -> Result<(HashSet<String>, HashMap<String, String>), ()> {
    let mut known = HashSet::new();
    let mut group_titles = HashMap::new();
    let names = contact_names(manager).await;
    let history_before = unix_now_millis().map(initial_history_before);
    let contacts = manager.store().contacts().await.map_err(|_| ())?;
    for contact in contacts.flatten() {
        let conversation = conversation_from_contact(&contact);
        known.insert(conversation.id.clone());
        emit_refreshed_row(events, rows, unread, conversation);
        let Some(before) = history_before else {
            continue;
        };
        let thread = Thread::Contact(ServiceId::Aci(contact.uuid.into()));
        let Ok((messages, _)) = fetch_history_page(manager, &thread, before, None).await else {
            continue;
        };
        for message in messages {
            emit_content(events, &message, &names, &group_titles, rows, Some(unread));
        }
    }
    let groups = manager.store().groups().await.map_err(|_| ())?;
    for group in groups.flatten() {
        let (key, group) = group;
        let conversation = conversation_from_group(&key, &group);
        session.remember_group(&key).await;
        group_titles.insert(conversation.id.clone(), conversation.title.clone());
        known.insert(conversation.id.clone());
        emit_refreshed_row(events, rows, unread, conversation);
        let Some(before) = history_before else {
            continue;
        };
        let thread = Thread::Group(key);
        let Ok((messages, _)) = fetch_history_page(manager, &thread, before, None).await else {
            continue;
        };
        for message in messages {
            emit_content(events, &message, &names, &group_titles, rows, Some(unread));
        }
    }
    Ok((known, group_titles))
}

#[allow(
    clippy::too_many_arguments,
    reason = "one history page needs the store, the session, and the inbox maps"
)]
async fn load_older_page(
    manager: &Manager<SqliteStore, Registered>,
    session: &Session,
    events: &EventTx,
    names: &HashMap<String, String>,
    group_titles: &HashMap<String, String>,
    conversation_id: &str,
    before_message_id: &str,
    rows: &mut HashMap<String, Conversation>,
) {
    let loaded = async {
        let before = message_millis(before_message_id)?;
        let thread = thread_of(conversation_id, session.group_key(conversation_id).await)?;
        fetch_history_page(manager, &thread, before, Some(before_message_id))
            .await
            .ok()
    }
    .await;
    let (page, more, note) = older_page_outcome(loaded);
    for message in page {
        emit_content(events, &message, names, group_titles, rows, None);
    }
    let _ = events.send(AdapterEvent::OlderHistoryLoaded {
        protocol: ProtocolId::Signal,
        conversation_id: conversation_id.to_string(),
        before_message_id: before_message_id.to_string(),
        more,
        note: note.map(str::to_string),
    });
}

/// First window of a history read, in milliseconds. A dense chat fills one
/// page inside it. A quiet chat widens the window until the page is full or
/// the read reaches the start of the thread.
const HISTORY_WINDOW_MS: u64 = 24 * 60 * 60 * 1000;
const HISTORY_READ_FAILED: &str =
    "Signal could not load older messages. The request can be tried again.";

fn older_page_outcome<T>(loaded: Option<(Vec<T>, bool)>) -> (Vec<T>, bool, Option<&'static str>) {
    match loaded {
        Some((page, more)) => (page, more, None),
        None => (Vec::new(), true, Some(HISTORY_READ_FAILED)),
    }
}

fn history_window(before: u64, span: u64) -> (u64, bool) {
    let start = before.saturating_sub(span);
    (start, start == 0)
}

/// Exclusive end of the first history page. The store orders by timestamp and
/// has no limit, so the page must start at the current time. One millisecond
/// past `now_millis` keeps a message stamped at that millisecond inside
/// `start..before`.
fn initial_history_before(now_millis: u64) -> u64 {
    now_millis.saturating_add(1)
}

fn unix_now_millis() -> Option<u64> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis();
    u64::try_from(millis).ok()
}

fn window_is_enough(count: usize, page: usize, reached_start: bool) -> bool {
    count >= page || reached_start
}

fn widen_history_span(before: u64, span: u64) -> u64 {
    let next = span.saturating_mul(2);
    if before.saturating_sub(next) == before.saturating_sub(span) {
        before
    } else {
        next
    }
}

async fn fetch_history_page(
    manager: &Manager<SqliteStore, Registered>,
    thread: &Thread,
    before: u64,
    before_id: Option<&str>,
) -> Result<(Vec<Content>, bool), ()> {
    let Some(url) = history_store_url() else {
        return Err(());
    };
    let pool = open_history_read(&url).await?;
    let outcome = async {
        let mut span = HISTORY_WINDOW_MS;
        let include_before = before_id.is_some();
        loop {
            let (start, reached_start) = history_window(before, span);
            let timestamps = read_history_timestamps(
                &pool,
                thread,
                start,
                before,
                include_before,
                HISTORY_READ_LIMIT,
            )
            .await?;
            // Each timestamp is one row (`message` loads that row). The timestamp
            // query stops at `HISTORY_READ_LIMIT`.
            let mut page = NewestBound::new(HISTORY_PAGE);
            for ts in timestamps {
                let Some(content) = manager.store().message(thread, ts).await.map_err(|_| ())?
                else {
                    continue;
                };
                let (millis, id) = content_history_key(&content);
                if is_on_history_page(millis, &id, before, before_id) {
                    page.consider(millis, id, content);
                }
            }
            if window_is_enough(page.passing(), HISTORY_PAGE, reached_start) {
                return Ok(page.finish(reached_start));
            }
            span = widen_history_span(before, span);
        }
    }
    .await;
    pool.close().await;
    outcome
}

fn history_store_url() -> Option<String> {
    let path = signal_session_path().ok()?;
    sqlite_store_path(&path).to_str().map(str::to_string)
}

async fn open_history_read(url: &str) -> Result<sqlx::SqlitePool, ()> {
    let options = url
        .parse::<sqlx::sqlite::SqliteConnectOptions>()
        .map_err(|_| ())?
        .create_if_missing(false)
        .read_only(true)
        .busy_timeout(std::time::Duration::from_secs(2));
    sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .map_err(|_| ())
}

/// Newest timestamps in the window, at most `limit` rows.
/// presage `messages()` loads every row of the window. This query stops early.
async fn read_history_timestamps(
    pool: &sqlx::SqlitePool,
    thread: &Thread,
    start: u64,
    end: u64,
    include_end: bool,
    limit: usize,
) -> Result<Vec<u64>, ()> {
    let (group_key, recipient) = thread_store_keys(thread);
    let start = i64::try_from(start).map_err(|_| ())?;
    let end = i64::try_from(end).map_err(|_| ())?;
    let limit = i64::try_from(limit).map_err(|_| ())?;
    let sql = if include_end {
        HISTORY_TIMESTAMPS_INCLUSIVE
    } else {
        HISTORY_TIMESTAMPS_EXCLUSIVE
    };
    let rows = sqlx::query(sql)
        .bind(group_key)
        .bind(recipient)
        .bind(start)
        .bind(end)
        .bind(limit)
        .fetch_all(pool)
        .await
        .map_err(|_| ())?;
    rows.iter()
        .map(|row| {
            let ts: i64 = sqlx::Row::try_get(row, 0).map_err(|_| ())?;
            u64::try_from(ts).map_err(|_| ())
        })
        .collect()
}

const HISTORY_TIMESTAMPS_EXCLUSIVE: &str = "\
SELECT ts FROM thread_messages \
WHERE thread_id = ( \
    SELECT id FROM threads WHERE group_master_key = ? OR recipient_id = ?) \
    AND ts >= ? AND ts < ? \
ORDER BY ts DESC \
LIMIT ?";

const HISTORY_TIMESTAMPS_INCLUSIVE: &str = "\
SELECT ts FROM thread_messages \
WHERE thread_id = ( \
    SELECT id FROM threads WHERE group_master_key = ? OR recipient_id = ?) \
    AND ts >= ? AND ts <= ? \
ORDER BY ts DESC \
LIMIT ?";

fn thread_store_keys(thread: &Thread) -> (Option<&[u8]>, Option<Uuid>) {
    match thread {
        Thread::Group(key) => (Some(key.as_slice()), None),
        Thread::Contact(service_id) => (None, Some(service_id.raw_uuid())),
    }
}

/// Query bounds for one history window.
/// An older page includes `before` so messages that share the cursor's
/// millisecond are still returned. The page drops the cursor itself.
#[cfg(test)]
fn history_bounds(
    start: u64,
    before: u64,
    include_before: bool,
) -> (std::ops::Bound<u64>, std::ops::Bound<u64>) {
    let end = if include_before {
        std::ops::Bound::Included(before)
    } else {
        std::ops::Bound::Excluded(before)
    };
    (std::ops::Bound::Included(start), end)
}

/// `true` when this row is strictly older than the cursor.
/// The id is `{millis}:{sender}`, so peers of one millisecond stay ordered.
fn is_on_history_page(millis: u64, id: &str, before: u64, before_id: Option<&str>) -> bool {
    match before_id {
        Some(cursor) => millis < before || (millis == before && id < cursor),
        None => millis < before,
    }
}

/// The newest `limit` rows, ordered oldest first. The buffer never grows
/// past `limit`, however many rows the window contains.
struct NewestBound<T> {
    rows: Vec<OrderedRow<T>>,
    limit: usize,
    overflow: bool,
}

struct OrderedRow<T> {
    millis: u64,
    id: String,
    value: T,
}

impl<T> NewestBound<T> {
    fn new(limit: usize) -> Self {
        Self {
            rows: Vec::new(),
            limit,
            overflow: false,
        }
    }

    /// How many passing rows were seen, at least `limit + 1` once the page filled.
    fn passing(&self) -> usize {
        if self.overflow {
            self.limit.saturating_add(1)
        } else {
            self.rows.len()
        }
    }

    fn consider(&mut self, millis: u64, id: String, value: T) {
        if self.limit == 0 {
            self.overflow = true;
            return;
        }
        if self.rows.iter().any(|row| row.id == id) {
            return;
        }
        let newer_than_oldest = self
            .rows
            .first()
            .is_none_or(|oldest| (millis, id.as_str()) > (oldest.millis, oldest.id.as_str()));
        if self.rows.len() == self.limit && !newer_than_oldest {
            self.overflow = true;
            return;
        }
        if self.rows.len() == self.limit {
            self.rows.remove(0);
            self.overflow = true;
        }
        let index = self
            .rows
            .iter()
            .position(|row| (row.millis, row.id.as_str()) > (millis, id.as_str()))
            .unwrap_or(self.rows.len());
        self.rows.insert(index, OrderedRow { millis, id, value });
    }

    fn finish(self, reached_start: bool) -> (Vec<T>, bool) {
        let more = self.overflow || !reached_start;
        (self.rows.into_iter().map(|row| row.value).collect(), more)
    }
}

fn content_history_key(content: &Content) -> (u64, String) {
    let millis = content_millis(content);
    let id = signal_message_id(millis, &contact_id(&content.metadata.sender));
    (millis, id)
}

fn content_millis(content: &Content) -> u64 {
    u64::try_from(content.metadata.client_timestamp.timestamp_millis()).unwrap_or(0)
}

fn thread_of(conversation_id: &str, group_key: Option<[u8; 32]>) -> Option<Thread> {
    if super::group::is_group_id(conversation_id) {
        return group_key.map(Thread::Group);
    }
    service_id_of(conversation_id).map(Thread::Contact)
}

fn service_id_of(conversation_id: &str) -> Option<ServiceId> {
    if let Some(rest) = conversation_id.strip_prefix("pni:") {
        let uuid = Uuid::parse_str(rest).ok()?;
        return Some(ServiceId::Pni(uuid.into()));
    }
    let uuid = Uuid::parse_str(conversation_id).ok()?;
    Some(ServiceId::Aci(uuid.into()))
}

fn contact_id(service_id: &ServiceId) -> String {
    match service_id {
        ServiceId::Pni(_) => format!("pni:{}", service_id.raw_uuid()),
        ServiceId::Aci(_) => service_id.raw_uuid().to_string(),
    }
}

fn signal_message_id(sent_millis: u64, sender: &str) -> String {
    format!("{sent_millis}:{sender}")
}

fn message_millis(id: &str) -> Option<u64> {
    id.split(':').next()?.parse().ok()
}

fn visible_title(name: &str, id: &str) -> String {
    if name.is_empty() {
        id.to_string()
    } else {
        name.to_string()
    }
}

fn live_unread(counts: &mut HashMap<String, u32>, id: &str, inbound: bool, viewed: bool) -> u32 {
    if !inbound || viewed {
        counts.insert(id.to_string(), 0);
        return 0;
    }
    let next = counts.get(id).copied().unwrap_or(0).saturating_add(1);
    counts.insert(id.to_string(), next);
    next
}

fn read_row(mut row: Conversation) -> Conversation {
    row.unread = 0;
    row
}

/// Store and emit a row rebuilt from the store.
/// The store row starts at unread zero. The count comes from the worker map.
fn emit_refreshed_row(
    events: &EventTx,
    rows: &mut HashMap<String, Conversation>,
    unread: &HashMap<String, u32>,
    mut conversation: Conversation,
) {
    conversation.unread = unread.get(&conversation.id).copied().unwrap_or(0);
    remember_row(rows, &conversation);
    emit_conversation(events, conversation);
}

/// Clear the unread count stored on a `Viewed` job.
/// `None` means the user left the open chat, so no badge changes.
fn clear_viewed(
    events: &EventTx,
    unread: &mut HashMap<String, u32>,
    rows: &mut HashMap<String, Conversation>,
    conversation_id: Option<String>,
) {
    let Some(id) = conversation_id else {
        return;
    };
    unread.insert(id.clone(), 0);
    if let Some(row) = rows.get(&id).cloned() {
        let row = read_row(row);
        rows.insert(id, row.clone());
        emit_conversation(events, row);
    }
}

fn remember_row(rows: &mut HashMap<String, Conversation>, conversation: &Conversation) {
    rows.insert(conversation.id.clone(), conversation.clone());
}

fn sent_sync_body(sync: &presage::libsignal_service::proto::SyncMessage) -> Option<&str> {
    use presage::libsignal_service::proto::sync_message::Content as SyncContent;
    match sync.content.as_ref()? {
        SyncContent::Sent(sent) => sent.message.as_ref()?.body.as_deref(),
        _ => None,
    }
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
        title: visible_title(&contact.name, &id),
        participant: id,
        preview: String::new(),
        unread: 0,
        order: i64::from(contact.inbox_position),
        last_at: 0,
        is_group: false,
        writable: true,
        muted: false,
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
        muted: false,
        placeholder: false,
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "a live envelope updates the row, the unread count, and the known set"
)]
fn emit_incoming(
    events: &EventTx,
    content: &Content,
    names: &HashMap<String, String>,
    group_titles: &HashMap<String, String>,
    known: &mut HashSet<String>,
    unread: &mut HashMap<String, u32>,
    viewed: Option<&str>,
    rows: &mut HashMap<String, Conversation>,
) {
    // The receive stream: a message that the server pushes now (#32).
    let Some((mut conversation, message)) = row_and_message(
        content,
        names,
        group_titles,
        thinwire_protocol::Arrival::Live,
    ) else {
        return;
    };
    let open = viewed == Some(conversation.id.as_str());
    conversation.unread = live_unread(unread, &conversation.id, !message.outbound, open);
    known.insert(conversation.id.clone());
    remember_row(rows, &conversation);
    emit_conversation(events, conversation);
    emit_message(events, message);
}

fn emit_content(
    events: &EventTx,
    content: &Content,
    names: &HashMap<String, String>,
    group_titles: &HashMap<String, String>,
    rows: &mut HashMap<String, Conversation>,
    unread: Option<&HashMap<String, u32>>,
) {
    // Stored messages (chat list, older pages): never notify.
    let Some((conversation, message)) = row_and_message(
        content,
        names,
        group_titles,
        thinwire_protocol::Arrival::History,
    ) else {
        return;
    };
    if let Some(unread) = unread {
        emit_refreshed_row(events, rows, unread, conversation);
    } else {
        remember_row(rows, &conversation);
        emit_conversation(events, conversation);
    }
    emit_message(events, message);
}

fn row_and_message(
    content: &Content,
    names: &HashMap<String, String>,
    group_titles: &HashMap<String, String>,
    arrival: thinwire_protocol::Arrival,
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
            let conversation_id = contact_id(id);
            let title = names
                .get(&conversation_id)
                .filter(|name| !name.is_empty())
                .cloned()
                .or_else(|| names.get(&uuid).filter(|name| !name.is_empty()).cloned())
                .unwrap_or_else(|| visible_title("", &conversation_id));
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
        id: signal_message_id(sent_millis, &contact_id(&content.metadata.sender)),
        sender: if shown.outbound {
            "me".to_string()
        } else {
            sender
        },
        body: shown.body.to_string(),
        outbound: shown.outbound,
        delivery: Delivery::Sent,
        sent_at: super::time::sent_at_secs(sent_millis),
        arrival,
    };
    let conversation = Conversation {
        protocol: ProtocolId::Signal,
        id: conversation_id.clone(),
        title,
        participant: conversation_id,
        preview: message.body.clone(),
        unread: 0,
        order: message.sent_at,
        last_at: message.sent_at,
        is_group,
        writable: true,
        muted: false,
        placeholder: false,
    };
    Some((conversation, message))
}

#[allow(
    clippy::too_many_arguments,
    reason = "a sent message refreshes the inbox row from the same maps as a live envelope"
)]
async fn send_text(
    manager: &mut Manager<SqliteStore, Registered>,
    session: &Session,
    outbound: &Outbound,
    events: &EventTx,
    names: &HashMap<String, String>,
    group_titles: &HashMap<String, String>,
    rows: &mut HashMap<String, Conversation>,
    unread: &mut HashMap<String, u32>,
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
        let service_id = service_id_of(&outbound.conversation_id).ok_or(())?;
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
    let sent_at = super::time::sent_at_secs(timestamp);
    let id = signal_message_id(timestamp, "me");
    let conversation = sent_conversation(outbound, sent_at, names, group_titles);
    unread.insert(conversation.id.clone(), 0);
    remember_row(rows, &conversation);
    emit_conversation(events, conversation);
    emit_message(
        events,
        ChatMessage {
            protocol: ProtocolId::Signal,
            conversation_id: outbound.conversation_id.clone(),
            id: id.clone(),
            sender: "me".into(),
            body: outbound.body.clone(),
            outbound: true,
            delivery: Delivery::Sent,
            sent_at,
            arrival: thinwire_protocol::Arrival::History,
        },
    );
    session.remember(&id, &outbound.body).await;
    Ok(())
}

fn sent_conversation(
    outbound: &Outbound,
    sent_at: i64,
    names: &HashMap<String, String>,
    group_titles: &HashMap<String, String>,
) -> Conversation {
    let id = outbound.conversation_id.clone();
    let is_group = super::group::is_group_id(&id);
    let title = if is_group {
        group_titles
            .get(&id)
            .cloned()
            .unwrap_or_else(|| visible_title("", &id))
    } else {
        names
            .get(&id)
            .filter(|name| !name.is_empty())
            .cloned()
            .unwrap_or_else(|| visible_title("", &id))
    };
    Conversation {
        protocol: ProtocolId::Signal,
        id: id.clone(),
        title,
        participant: id,
        preview: outbound.body.clone(),
        unread: 0,
        order: sent_at,
        last_at: sent_at,
        is_group,
        writable: true,
        muted: false,
        placeholder: false,
    }
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
        events_for_view(content, names, None, &mut HashMap::new())
    }

    fn events_for_view(
        content: &Content,
        names: &HashMap<String, String>,
        viewed: Option<&str>,
        unread: &mut HashMap<String, u32>,
    ) -> Vec<AdapterEvent> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut known = HashSet::new();
        let mut rows = HashMap::new();
        emit_incoming(
            &tx,
            content,
            names,
            &HashMap::new(),
            &mut known,
            unread,
            viewed,
            &mut rows,
        );
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    #[test]
    fn the_receive_stream_is_live_and_stored_messages_are_not() {
        let body = DataMessage {
            body: Some("hi".into()),
            ..Default::default()
        };
        let content = envelope(Uuid::from_u128(7), body);
        let names = HashMap::new();
        let arrival_of = |events: Vec<AdapterEvent>| {
            events.into_iter().find_map(|event| match event {
                AdapterEvent::MessageReceived { message } => Some(message.arrival),
                _ => None,
            })
        };
        assert_eq!(
            arrival_of(events_for(&content, &names)),
            Some(thinwire_protocol::Arrival::Live)
        );
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut rows = HashMap::new();
        emit_content(&tx, &content, &names, &HashMap::new(), &mut rows, None);
        let mut stored = Vec::new();
        while let Ok(event) = rx.try_recv() {
            stored.push(event);
        }
        assert_eq!(
            arrival_of(stored),
            Some(thinwire_protocol::Arrival::History),
            "a stored message never notifies"
        );
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
        let mut unread = HashMap::new();
        let mut rows = HashMap::new();
        emit_incoming(
            &tx,
            &content,
            &HashMap::new(),
            &HashMap::new(),
            &mut known,
            &mut unread,
            None,
            &mut rows,
        );
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
        let mut page = NewestBound::new(50);
        for n in 1..=60 {
            page.consider(n, n.to_string(), n);
        }
        assert!(page.overflow);
        assert_eq!(page.rows.first().map(|row| row.value), Some(11));
        assert_eq!(page.rows.last().map(|row| row.value), Some(60));
        let mut short = NewestBound::new(50);
        for n in 1..=10 {
            short.consider(n, n.to_string(), n);
        }
        assert!(!short.overflow);
        assert_eq!(short.rows.len(), 10);
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
        let mut unread = HashMap::new();
        let mut rows = HashMap::new();
        emit_incoming(
            &tx,
            &content,
            &HashMap::new(),
            &titles,
            &mut known,
            &mut unread,
            None,
            &mut rows,
        );
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

    #[test]
    fn two_senders_at_one_millisecond_keep_distinct_ids() {
        let first = Uuid::from_u128(0x1111);
        let second = Uuid::from_u128(0x2222);
        let body = DataMessage {
            body: Some("same time".into()),
            ..Default::default()
        };
        let left = events_for(&envelope(first, body.clone()), &HashMap::new());
        let right = events_for(&envelope(second, body), &HashMap::new());
        let left_id = message_id(&left);
        let right_id = message_id(&right);
        assert_ne!(left_id, right_id);
        assert_eq!(message_millis(&left_id), message_millis(&right_id));
        assert!(left_id.contains(&first.to_string()));
        assert!(right_id.contains(&second.to_string()));
    }

    #[test]
    fn a_pni_contact_keeps_its_service_id() {
        let uuid = Uuid::from_u128(0x3333_3333_3333_3333_3333_3333_3333_3333);
        let content = Content::from_body(
            DataMessage {
                body: Some("from a phone number".into()),
                ..Default::default()
            },
            Metadata {
                sender: ServiceId::Pni(uuid.into()),
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
        );
        let events = events_for(&content, &HashMap::new());
        let id = format!("pni:{uuid}");
        assert!(matches!(
            events.first(),
            Some(AdapterEvent::ConversationUpsert { conversation }) if conversation.id == id
        ));
        let parsed = service_id_of(&id).expect("service id");
        assert!(matches!(parsed, ServiceId::Pni(_)));
        assert_eq!(parsed.raw_uuid(), uuid);
    }

    #[test]
    fn an_open_chat_stays_read_and_a_closed_chat_counts() {
        let contact = Uuid::from_u128(0x4444);
        let body = DataMessage {
            body: Some("ping".into()),
            ..Default::default()
        };
        let content = envelope(contact, body);
        let mut unread = HashMap::new();
        let open = events_for_view(
            &content,
            &HashMap::new(),
            Some(&contact.to_string()),
            &mut unread,
        );
        assert!(matches!(
            open.first(),
            Some(AdapterEvent::ConversationUpsert { conversation }) if conversation.unread == 0
        ));
        let mut unread = HashMap::new();
        let first = events_for_view(&content, &HashMap::new(), None, &mut unread);
        let second = events_for_view(&content, &HashMap::new(), None, &mut unread);
        assert!(matches!(
            first.first(),
            Some(AdapterEvent::ConversationUpsert { conversation }) if conversation.unread == 1
        ));
        assert!(matches!(
            second.first(),
            Some(AdapterEvent::ConversationUpsert { conversation }) if conversation.unread == 2
        ));
    }

    #[test]
    fn a_sent_reply_refreshes_the_row() {
        let id = "chat-1".to_string();
        let outbound = Outbound {
            conversation_id: id.clone(),
            body: "sent line".into(),
            request: 7,
        };
        let mut names = HashMap::new();
        names.insert(id.clone(), "Ada".into());
        let row = sent_conversation(&outbound, 42, &names, &HashMap::new());
        assert_eq!(row.preview, "sent line");
        assert_eq!(row.title, "Ada");
        assert_eq!(row.unread, 0);
        assert_eq!(row.last_at, 42);
        assert_eq!(row.order, 42);
    }

    #[test]
    fn an_unnamed_contact_uses_its_id_as_the_title() {
        assert_eq!(visible_title("", "abc"), "abc");
        assert_eq!(visible_title("Ada", "abc"), "Ada");
    }

    #[test]
    fn the_first_history_page_uses_the_current_time() {
        let now = 1_700_000_000_000;
        let before = initial_history_before(now);
        assert_eq!(before, now + 1);
        let (start, reached_start) = history_window(before, HISTORY_WINDOW_MS);
        assert!(!reached_start);
        assert_eq!(start, before - HISTORY_WINDOW_MS);
        assert!(now >= start && now < before);
        let older_than_the_window = now - HISTORY_WINDOW_MS - 1;
        assert!(older_than_the_window < start);
        let wider = widen_history_span(before, HISTORY_WINDOW_MS);
        let (wider_start, wider_reached) = history_window(before, wider);
        assert!(!wider_reached);
        assert!(wider_start > 0);
        assert!(wider_start < start);
    }

    #[test]
    fn a_history_window_stays_bounded_until_the_page_fills() {
        let before = 10 * HISTORY_WINDOW_MS;
        let (start, reached) = history_window(before, HISTORY_WINDOW_MS);
        assert_eq!(start, before - HISTORY_WINDOW_MS);
        assert!(!reached);
        assert!(!window_is_enough(3, HISTORY_PAGE, false));
        assert!(window_is_enough(HISTORY_PAGE, HISTORY_PAGE, false));
        assert!(window_is_enough(1, HISTORY_PAGE, true));
        let wider = widen_history_span(before, HISTORY_WINDOW_MS);
        assert!(wider > HISTORY_WINDOW_MS);
        assert!(wider < before);
    }

    #[test]
    fn messages_that_share_the_boundary_millisecond_stay_reachable() {
        use std::ops::RangeBounds;

        let rows: Vec<(u64, String)> = (0..120)
            .map(|index| (5_000, format!("5000:s{index:03}")))
            .collect();
        assert!(!history_bounds(0, 5_001, false).contains(&5_001));
        assert!(history_bounds(0, 5_000, true).contains(&5_000));
        assert!(!history_bounds(0, 5_000, true).contains(&5_001));

        let mut before = 5_001;
        let mut before_id = None;
        let mut seen = Vec::new();
        for _ in 0..5 {
            let (page, high_water) = select_history_ids(&rows, before, before_id.as_deref());
            assert!(high_water <= HISTORY_PAGE);
            assert!(!page.is_empty());
            for id in &page {
                assert!(!seen.contains(id), "{id} returned twice");
            }
            let oldest = page.first().expect("oldest").clone();
            before = message_millis(&oldest).expect("cursor millis");
            before_id = Some(oldest);
            seen.extend(page);
            if seen.len() == rows.len() {
                break;
            }
        }
        let mut expected: Vec<String> = rows.into_iter().map(|(_, id)| id).collect();
        expected.sort();
        let mut got = seen;
        got.sort();
        assert_eq!(got, expected);
        assert_eq!(before_id.as_deref(), Some("5000:s000"));
    }

    #[test]
    fn a_history_read_keeps_at_most_one_page_of_rows() {
        let mut page = NewestBound::new(HISTORY_PAGE);
        let mut high_water = 0;
        for index in 0..10_000u64 {
            let id = format!("{index}:sender");
            page.consider(index, id, ());
            high_water = high_water.max(page.rows.len());
        }
        assert!(high_water <= HISTORY_PAGE);
        assert_eq!(page.rows.len(), HISTORY_PAGE);
        assert!(page.overflow);
        assert_eq!(
            page.rows.first().expect("oldest").millis,
            10_000 - HISTORY_PAGE as u64
        );
        assert_eq!(page.rows.last().expect("newest").millis, 9_999);

        let mut burst = NewestBound::new(HISTORY_PAGE);
        let mut burst_high = 0;
        for index in 0..5_000 {
            let id = format!("8000:s{index:04}");
            if is_on_history_page(8_000, &id, 8_001, None) {
                burst.consider(8_000, id, ());
            }
            burst_high = burst_high.max(burst.rows.len());
        }
        assert!(burst_high <= HISTORY_PAGE);
        assert_eq!(burst.rows.len(), HISTORY_PAGE);
        assert_eq!(
            burst.rows.first().expect("oldest peer").id,
            format!("8000:s{:04}", 5_000 - HISTORY_PAGE)
        );
    }

    /// #175: a window with more rows than one page still returns a bounded read.
    #[tokio::test]
    async fn a_history_page_reads_a_bounded_number_of_rows() {
        let path = std::env::temp_dir().join(format!(
            "thinwire-signal-history-{}.sqlite",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let url = path.to_str().expect("temp path");
        let options = url
            .parse::<sqlx::sqlite::SqliteConnectOptions>()
            .expect("url")
            .create_if_missing(true);
        let pool = sqlx::SqlitePool::connect_with(options).await.expect("db");
        sqlx::query(
            "CREATE TABLE threads (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                group_master_key BLOB UNIQUE,
                recipient_id BLOB
            )",
        )
        .execute(&pool)
        .await
        .expect("threads");
        sqlx::query(
            "CREATE TABLE thread_messages (
                ts INTEGER NOT NULL,
                thread_id INTEGER NOT NULL,
                PRIMARY KEY (ts, thread_id)
            )",
        )
        .execute(&pool)
        .await
        .expect("messages");
        let key = [9u8; 32];
        sqlx::query("INSERT INTO threads (group_master_key) VALUES (?)")
            .bind(key.as_slice())
            .execute(&pool)
            .await
            .expect("thread");
        let thread_id: i64 =
            sqlx::query_scalar("SELECT id FROM threads WHERE group_master_key = ?")
                .bind(key.as_slice())
                .fetch_one(&pool)
                .await
                .expect("thread id");
        let stored = 200i64;
        for ts in 1..=stored {
            sqlx::query("INSERT INTO thread_messages (ts, thread_id) VALUES (?, ?)")
                .bind(ts)
                .bind(thread_id)
                .execute(&pool)
                .await
                .expect("row");
        }
        let thread = Thread::Group(key);
        let got = super::read_history_timestamps(
            &pool,
            &thread,
            0,
            u64::try_from(stored).expect("end") + 1,
            false,
            HISTORY_READ_LIMIT,
        )
        .await
        .expect("bounded read");
        pool.close().await;
        let _ = std::fs::remove_file(&path);
        assert!(
            got.len() <= HISTORY_READ_LIMIT,
            "the read stays within the limit"
        );
        assert!(
            got.len() < stored as usize,
            "the window holds more than one page"
        );
        assert_eq!(got.len(), HISTORY_READ_LIMIT);
        assert_eq!(got.first().copied(), Some(stored as u64));
        assert_eq!(
            got.last().copied(),
            Some(stored as u64 - HISTORY_READ_LIMIT as u64 + 1)
        );
    }

    fn select_history_ids(
        rows: &[(u64, String)],
        before: u64,
        before_id: Option<&str>,
    ) -> (Vec<String>, usize) {
        let mut page = NewestBound::new(HISTORY_PAGE);
        let mut high_water = 0;
        for (millis, id) in rows {
            if is_on_history_page(*millis, id, before, before_id) {
                page.consider(*millis, id.clone(), id.clone());
            }
            high_water = high_water.max(page.rows.len());
        }
        let ids = page.rows.iter().map(|row| row.id.clone()).collect();
        (ids, high_water)
    }

    #[test]
    fn a_failed_history_read_stays_retryable() {
        let (page, more, note) = older_page_outcome::<u8>(None);
        assert!(page.is_empty());
        assert!(more);
        assert_eq!(note, Some(HISTORY_READ_FAILED));
        let (page, more, note) = older_page_outcome(Some((vec![1], false)));
        assert_eq!(page, vec![1]);
        assert!(!more);
        assert!(note.is_none());
    }

    #[test]
    fn a_refresh_keeps_the_worker_unread_count() {
        let contact = Uuid::from_u128(0x5555);
        let id = contact.to_string();
        let mut unread = HashMap::new();
        unread.insert(id.clone(), 4);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut rows = HashMap::new();
        emit_refreshed_row(&tx, &mut rows, &unread, inbox_row(&id, 0));
        let content = envelope(
            contact,
            DataMessage {
                body: Some("stored line".into()),
                ..Default::default()
            },
        );
        emit_content(
            &tx,
            &content,
            &HashMap::new(),
            &HashMap::new(),
            &mut rows,
            Some(&unread),
        );
        let mut counts = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let AdapterEvent::ConversationUpsert { conversation } = event {
                assert_eq!(conversation.id, id);
                counts.push(conversation.unread);
            }
        }
        assert_eq!(counts, vec![4, 4]);
        assert_eq!(rows.get(&id).expect("row").unread, 4);
    }

    #[test]
    fn two_queued_viewed_jobs_clear_their_own_chats() {
        let session = Session::new();
        let mut jobs = session.install_jobs_for_test();
        session.set_viewed(Some("b".to_string()));
        assert!(session.request_viewed(Some("a".to_string())));
        assert!(session.request_viewed(Some("b".to_string())));
        assert_eq!(session.viewed().as_deref(), Some("b"));

        let mut unread = HashMap::from([("a".to_string(), 2), ("b".to_string(), 5)]);
        let mut rows = HashMap::from([
            ("a".to_string(), inbox_row("a", 2)),
            ("b".to_string(), inbox_row("b", 5)),
        ]);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut queued = Vec::new();
        for _ in 0..2 {
            let Ok(WorkerJob::Viewed(id)) = jobs.try_recv() else {
                panic!("queued job must carry the viewed chat");
            };
            queued.push(id);
        }
        assert!(jobs.try_recv().is_err());
        assert_eq!(queued, vec![Some("a".to_string()), Some("b".to_string())]);
        for id in queued {
            clear_viewed(&tx, &mut unread, &mut rows, id);
        }
        assert_eq!(unread.get("a"), Some(&0));
        assert_eq!(unread.get("b"), Some(&0));
        assert_eq!(rows.get("a").expect("a").unread, 0);
        assert_eq!(rows.get("b").expect("b").unread, 0);
        let mut cleared = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let AdapterEvent::ConversationUpsert { conversation } = event {
                cleared.push((conversation.id, conversation.unread));
            }
        }
        assert_eq!(cleared, vec![("a".to_string(), 0), ("b".to_string(), 0)]);
    }

    fn inbox_row(id: &str, unread: u32) -> Conversation {
        Conversation {
            protocol: ProtocolId::Signal,
            id: id.to_string(),
            title: id.to_string(),
            participant: id.to_string(),
            preview: String::new(),
            unread,
            order: 0,
            last_at: 0,
            is_group: false,
            writable: true,
            muted: false,
            placeholder: false,
        }
    }

    fn message_id(events: &[AdapterEvent]) -> String {
        events
            .iter()
            .find_map(|event| match event {
                AdapterEvent::MessageReceived { message } => Some(message.id.clone()),
                _ => None,
            })
            .expect("message")
    }
}
