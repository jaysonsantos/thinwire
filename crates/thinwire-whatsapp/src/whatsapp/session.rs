// SPDX-License-Identifier: AGPL-3.0-only
//! Shared WhatsApp session state between the adapter and the live client.
//!
//! The live client (feature `whatsapp-web`) turns whatsapp-rust events into
//! [`LinkEvent`] values. Tests feed the same values from fakes. Pairing
//! material travels only inside [`RedactedPairingSecret`].

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use super::inbox::{
    self, HistoryChat, Inbox, MediaRef, MediaRequest, Mute, TargetRef, WaContact, WaMessage,
    WaReaction,
};
use thinwire_protocol::{
    AccountState, AdapterEvent, AdapterStatus, EventTx, ProtocolId, RedactedPairingSecret,
    emit_status,
};

pub(super) const PAIRED: &str =
    "WhatsApp device linked. Connecting. Experimental. Ban risk. Not a supported messenger.";
pub(super) const CONNECTED: &str =
    "Experimental WhatsApp linked device is connected. Ban risk. Not a supported messenger.";
pub(super) const DISCONNECTED: &str =
    "Experimental WhatsApp connection dropped. The client tries to connect again.";
pub(super) const LOGGED_OUT: &str =
    "WhatsApp unlinked this device. Open the ban gate again to pair a new session.";
pub(super) const TEMPORARY_BAN: &str =
    "WhatsApp put a temporary ban on this account. Stop using the experimental client.";
pub(super) const PAIR_FAILED: &str = "WhatsApp pairing failed. Cancel, then try again.";
pub(super) const PAIR_THROTTLED: &str = "WhatsApp limited pair-code requests. Wait some minutes, then cancel and try again. Repeated tries raise the ban risk.";
pub(super) const SEND_NETWORK: &str = "WhatsApp send failed: no connection. Nothing was sent. Try again when the status is connected.";
pub(super) const SEND_UNLINKED: &str = "WhatsApp unlinked this device. Nothing was sent. Open the ban gate again to pair a new session.";
pub(super) const SEND_REJECTED: &str = "WhatsApp did not accept the message. Nothing was sent.";
pub(super) const QR_EXHAUSTED: &str = thinwire_protocol::WHATSAPP_QR_EXHAUSTED;

const HISTORY_WAIT: Duration = Duration::from_secs(20);

const OLDER_HISTORY_FAILED: &str = "Could not load older WhatsApp messages.";

/// Boxed send future. Resolves to the server message id.
pub(super) type SendFuture<'a> =
    Pin<Box<dyn Future<Output = Result<String, SendFailure>> + Send + 'a>>;

/// Send failure class without server text, JIDs, or bodies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(any(test, feature = "whatsapp-web")), allow(dead_code))]
pub(super) enum SendFailure {
    /// Transport or IQ failure. The client reconnects on its own.
    Network,
    /// The phone revoked this linked device.
    Unlinked,
    /// The server or the client refused the message.
    Rejected,
}

impl SendFailure {
    #[must_use]
    pub(super) const fn detail(self) -> &'static str {
        match self {
            Self::Network => SEND_NETWORK,
            Self::Unlinked => SEND_UNLINKED,
            Self::Rejected => SEND_REJECTED,
        }
    }
}

/// Outgoing text transport. The live client wraps whatsapp-rust. Tests use a fake.
pub(super) trait WhatsAppSender: Send + Sync {
    fn send_text<'a>(&'a self, chat_jid: &'a str, body: &'a str) -> SendFuture<'a>;

    /// Upload one photo or video and send it, with `caption` when it is not
    /// empty. Resolves to the server message id. A sender with no client
    /// refuses.
    fn send_media<'a>(
        &'a self,
        _chat_jid: &'a str,
        _file: &'a thinwire_protocol::OutgoingFile,
        _caption: &'a str,
    ) -> SendFuture<'a> {
        Box::pin(async { Err(SendFailure::Rejected) })
    }

    /// Download and decrypt one media file. A sender with no client has
    /// nothing to download.
    fn download<'a>(&'a self, _media: &'a MediaRef) -> DownloadFuture<'a> {
        Box::pin(async { Err(()) })
    }

    /// The subject of each group of the account, as `(jid, subject)`.
    fn group_names(&self) -> NamesFuture<'_> {
        Box::pin(async { Ok(Vec::new()) })
    }

    /// The small profile picture of a contact or a group, as image bytes.
    /// `Err` when there is none, or when the privacy of the contact hides it.
    fn profile_picture<'a>(&'a self, _jid: &'a str) -> DownloadFuture<'a> {
        Box::pin(async { Err(()) })
    }

    /// Send text that quotes `quote`. A sender with no quotes sends plain
    /// text.
    fn send_reply<'a>(
        &'a self,
        chat_jid: &'a str,
        body: &'a str,
        _quote: &'a TargetRef,
    ) -> SendFuture<'a> {
        self.send_text(chat_jid, body)
    }

    /// Set the reaction `emoji` of this account on `target`. An empty
    /// `emoji` removes it. A sender with no client refuses.
    fn react<'a>(
        &'a self,
        _chat_jid: &'a str,
        _target: &'a TargetRef,
        _emoji: &'a str,
    ) -> ActionFuture<'a> {
        Box::pin(async { Err(()) })
    }

    /// Delete `target` for every member of the chat (a revoke).
    fn revoke<'a>(&'a self, _chat_jid: &'a str, _target: &'a TargetRef) -> ActionFuture<'a> {
        Box::pin(async { Err(()) })
    }

    /// Delete `target` on the devices of this account only.
    fn delete_for_me<'a>(&'a self, _chat_jid: &'a str, _target: &'a TargetRef) -> ActionFuture<'a> {
        Box::pin(async { Err(()) })
    }

    fn group_subject<'a>(&'a self, _jid: &'a str) -> SubjectFuture<'a> {
        Box::pin(async { Ok(None) })
    }

    /// The phone JID of a LID, from the device store. `None` when the store
    /// has no mapping or the lookup fails.
    fn phone_of<'a>(&'a self, _lid: &'a str) -> PhoneFuture<'a> {
        Box::pin(async { None })
    }

    /// Record the id before [`Self::request_history`]. The history
    /// notification can arrive before that send finishes.
    fn allocate_history_id(&self) -> Option<String> {
        None
    }

    /// The messages arrive later as a history sync that carries
    /// `ask.request_id`.
    fn request_history<'a>(&'a self, _ask: &'a HistoryAsk<'a>) -> ActionFuture<'a> {
        Box::pin(async { Err(()) })
    }
}

#[cfg_attr(not(feature = "whatsapp-web"), allow(dead_code))]
pub(super) struct HistoryAsk<'a> {
    pub chat_jid: &'a str,
    pub oldest_id: &'a str,
    pub oldest_from_me: bool,
    pub oldest_timestamp_ms: i64,
    pub count: i32,
    pub request_id: &'a str,
}

/// Boxed message action: a reaction or a delete. The error has no text.
pub(super) type ActionFuture<'a> = Pin<Box<dyn Future<Output = Result<(), ()>> + Send + 'a>>;

/// Boxed group list. The error has no text.
pub(super) type NamesFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<(String, String)>, ()>> + Send + 'a>>;

pub(super) type SubjectFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Option<String>, ()>> + Send + 'a>>;

pub(super) type PhoneFuture<'a> = Pin<Box<dyn Future<Output = Option<String>> + Send + 'a>>;

/// Chat pictures that one run fetches: the newest chats only. A lookup for
/// every chat at each start is a load that WhatsApp can see.
const AVATAR_CHATS: usize = 50;

/// The most mentioned LIDs that one pass asks the device store for.
const PHONE_ASKS: usize = 200;

/// Picture lookups that run at one time.
const AVATAR_LOOKUPS: usize = 2;

/// Boxed media download. Resolves to the plain bytes. The error has no text.
pub(super) type DownloadFuture<'a> = Pin<Box<dyn Future<Output = Result<Vec<u8>, ()>> + Send + 'a>>;

/// Sticker downloads that run at one time.
const MEDIA_DOWNLOADS: usize = 4;

/// What the linked-device client reports to the adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(feature = "whatsapp-web"), allow(dead_code))]
pub(super) enum LinkEvent {
    Qr(RedactedPairingSecret),
    PairCode(RedactedPairingSecret),
    PairFailed,
    /// The server refused a pair-code request as rate limited.
    PairThrottled,
    QrExhausted,
    Paired,
    Connected,
    Disconnected,
    LoggedOut,
    TemporaryBan,
    History {
        chats: Vec<HistoryChat>,
        push_names: Vec<(String, String)>,
        /// `None` on a server push. `Some` answers `request_history`.
        request_id: Option<String>,
        ended: Vec<String>,
    },
    Messages(Vec<WaMessage>),
    /// Live reactions to cached messages.
    Reactions(Vec<WaReaction>),
    /// The phone muted or unmuted a chat (app-state `MuteAction`).
    Mute {
        jid: String,
        mute: Mute,
    },
    /// The phone saved, changed, or removed address-book contacts
    /// (app-state `ContactAction`).
    Contacts(Vec<WaContact>),
}

/// A connected sender and the link generation it belongs to.
pub(super) type LinkSender = (u64, Arc<dyn WhatsAppSender>);

/// Session generation while no link runs. Link tokens start at 1.
const NO_LINK: u64 = 0;

#[derive(Default)]
struct State {
    inbox: Inbox,
    connected: bool,
    sender: Option<Arc<dyn WhatsAppSender>>,
    /// Link generation that may install a sender. Set by [`Session::begin`].
    generation: u64,
    /// The shell's pairing id. QR codes and pair codes carry it.
    pairing: u64,
    /// Status text of the event that stopped the link. Cleared on a new link.
    stopped: Option<&'static str>,
    /// This link sent `Account { Linked }`. Until then the shell drops inbox
    /// events (ADR 0010 rule 1), so the session keeps them in the inbox and
    /// sends the chat page on `Connected`.
    announced: bool,
    /// The timer for the next timed mute end. One timer runs at a time
    /// (#168 item 13).
    mute_timer: Option<MuteTimer>,
    /// Number of the last mute timer.
    mute_timers: u64,
    /// The adapter shut down. A send result that comes later is dropped, so
    /// no `SendAccepted`, `SendRejected` or row follows `Stopped` (#238).
    closed: bool,
    /// Limits the sticker downloads that run at one time.
    media_gate: Option<Arc<tokio::sync::Semaphore>>,
    /// This link fetched its chat pictures. A reconnect does not again.
    avatars_asked: bool,
    subjects_listed: bool,
    /// Groups this link already asked for a subject. A query that fails
    /// or returns no name stays here until `begin` or `reset`. A socket
    /// reconnect does not clear it, so a miss is not asked again then.
    subjects_asked: HashSet<String>,
    older_waits: HashMap<String, OlderWait>,
}

struct OlderWait {
    conversation_id: String,
    before: String,
    jid: String,
    timer: Option<tokio::task::AbortHandle>,
}

/// The timer for the next timed mute end (#168 item 13).
struct MuteTimer {
    /// The mute end it waits for (Unix ms).
    end: i64,
    /// Its number. A task that is no longer the armed timer does nothing:
    /// `abort` cannot stop a task that already waits for the lock
    /// (Codex r4139029620).
    id: u64,
    task: tokio::task::JoinHandle<()>,
}

impl LinkEvent {
    /// The client must stop: no reconnect after a logout, a ban, or a dead pairing.
    #[must_use]
    #[cfg_attr(not(any(test, feature = "whatsapp-web")), allow(dead_code))]
    pub(super) const fn stops_link(&self) -> bool {
        matches!(
            self,
            Self::LoggedOut
                | Self::TemporaryBan
                | Self::PairFailed
                | Self::PairThrottled
                | Self::QrExhausted
        )
    }

    /// The phone revoked this device. The saved device store is no longer valid.
    #[must_use]
    #[cfg_attr(not(any(test, feature = "whatsapp-web")), allow(dead_code))]
    pub(super) const fn invalidates_device(&self) -> bool {
        matches!(self, Self::LoggedOut)
    }

    const fn stop_detail(&self) -> Option<&'static str> {
        Some(match self {
            Self::LoggedOut => LOGGED_OUT,
            Self::TemporaryBan => TEMPORARY_BAN,
            Self::PairFailed => PAIR_FAILED,
            Self::PairThrottled => PAIR_THROTTLED,
            Self::QrExhausted => QR_EXHAUSTED,
            _ => return None,
        })
    }
}

/// Inbox plus link state. Cheap to clone; all clones share one state.
#[derive(Clone, Default)]
pub(super) struct Session {
    state: Arc<Mutex<State>>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session").finish_non_exhaustive()
    }
}

impl Session {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[cfg_attr(not(any(test, feature = "whatsapp-web")), allow(dead_code))]
    /// Install the sender of link `generation`. A start from an older
    /// generation can finish late; its sender is ignored, so it cannot
    /// replace the sender of the current link. Returns `true` if stored.
    pub(super) fn attach_sender(&self, generation: u64, sender: Arc<dyn WhatsAppSender>) -> bool {
        let mut state = self.lock();
        if state.generation != generation {
            return false;
        }
        state.sender = Some(sender);
        true
    }

    #[must_use]
    pub(super) fn is_connected(&self) -> bool {
        self.lock().connected
    }

    /// Connected sender and its link generation, or `None` while pairing or
    /// offline. Pass the generation to [`Self::finish_send`].
    #[must_use]
    pub(super) fn sender(&self) -> Option<LinkSender> {
        let state = self.lock();
        if state.connected {
            state
                .sender
                .clone()
                .map(|sender| (state.generation, sender))
        } else {
            None
        }
    }

    /// Stop the link and forget the inbox. Returns removals for the UI.
    ///
    /// Events and send results of the old link are stale from here on.
    pub(super) fn reset(&self) -> Vec<AdapterEvent> {
        Self::reset_state(&mut self.lock())
    }

    fn reset_state(state: &mut State) -> Vec<AdapterEvent> {
        Self::abort_mute_timer(state);
        state.connected = false;
        state.sender = None;
        state.stopped = None;
        state.generation = NO_LINK;
        state.avatars_asked = false;
        state.subjects_listed = false;
        state.subjects_asked.clear();
        Self::clear_older_waits(state);
        let removed = state.inbox.clear();
        // Rows the shell never saw need no removal.
        if std::mem::take(&mut state.announced) {
            removed
        } else {
            Vec::new()
        }
    }

    /// Status text of the event that stopped the last link, if any.
    #[must_use]
    pub(super) fn stopped(&self) -> Option<&'static str> {
        self.lock().stopped
    }

    /// A new link starts. Forget the last stop reason and the old sender.
    #[cfg_attr(not(feature = "whatsapp-web"), allow(dead_code))]
    pub(super) fn begin(&self, generation: u64) {
        let mut state = self.lock();
        state.stopped = None;
        state.generation = generation;
        state.sender = None;
        state.announced = false;
        state.subjects_listed = false;
        state.subjects_asked.clear();
        Self::clear_older_waits(&mut state);
        // A timer of the old link has a stale generation: it would never end
        // a mute of this link (Codex r4138502157).
        Self::abort_mute_timer(&mut state);
        state.inbox.new_connection();
    }

    /// The adapter shuts down. The adapter calls it right before `Stopped`.
    ///
    /// It stops the mute timer, so no row comes after `Stopped` and the task
    /// lets go of the session and the event sender (Codex r4139029620). It
    /// also seals the sends: a send that outlived the shutdown drain and
    /// ends later publishes nothing (#238). [`Self::finish_send`] checks the
    /// seal under the same lock, so a result is either out before `Stopped`
    /// or never.
    pub(super) fn close(&self) {
        let mut state = self.lock();
        state.closed = true;
        Self::abort_mute_timer(&mut state);
    }

    fn abort_mute_timer(state: &mut State) {
        if let Some(timer) = state.mute_timer.take() {
            timer.task.abort();
        }
    }

    /// The adapter shut down.
    #[must_use]
    pub(super) fn is_closed(&self) -> bool {
        self.lock().closed
    }

    /// The session still belongs to link `generation`: no cancel reset it
    /// and no newer link began.
    #[must_use]
    pub(super) fn is_link(&self, generation: u64) -> bool {
        let state = self.lock();
        state.generation != NO_LINK && state.generation == generation
    }

    /// The shell's id for the pairing that runs now (`WhatsAppBeginLink`).
    #[cfg_attr(not(any(test, feature = "whatsapp-web")), allow(dead_code))]
    pub(super) fn set_pairing(&self, pairing: u64) {
        self.lock().pairing = pairing;
    }

    /// Owners of the session state: the adapter, the link owner, and each
    /// running timer or send task.
    #[cfg(test)]
    pub(super) fn owners(&self) -> usize {
        Arc::strong_count(&self.state)
    }

    #[cfg(test)]
    pub(super) fn has_mute_timer(&self) -> bool {
        self.lock().mute_timer.is_some()
    }

    pub(super) fn with_inbox<T>(&self, action: impl FnOnce(&mut Inbox) -> T) -> T {
        action(&mut self.lock().inbox)
    }

    /// Start the sticker downloads for the rows in `rows`. Each result sends
    /// its row again, with the image.
    pub(super) fn fetch_media(&self, rows: &[AdapterEvent], events: &EventTx) {
        let mut state = self.lock();
        self.spawn_media(&mut state, rows, true, events);
    }

    /// Nothing starts while the link is offline, closed, or not yet shown:
    /// the rows ask again when the chat opens.
    fn spawn_media(
        &self,
        state: &mut State,
        rows: &[AdapterEvent],
        history_photos: bool,
        events: &EventTx,
    ) {
        if state.closed || !state.connected || !state.announced {
            return;
        }
        let Some(sender) = state.sender.clone() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let requests = state.inbox.media_requests(rows, history_photos);
        if requests.is_empty() {
            return;
        }
        let gate = state
            .media_gate
            .get_or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(MEDIA_DOWNLOADS)))
            .clone();
        let generation = state.generation;
        for request in requests {
            let session = self.clone();
            let sender = sender.clone();
            let events = events.clone();
            let gate = gate.clone();
            runtime.spawn(async move {
                let _permit = gate.acquire_owned().await.ok();
                let bytes = sender.download(&request.media).await.ok();
                session.finish_media(generation, &request, bytes, &events);
            });
        }
    }

    /// Download the audio `file_id` into the voice folder. The answer is
    /// `VoiceReady`, or `VoiceFailed` when the link is down, the message left
    /// the cache, or the download or the write failed.
    pub(super) fn download_voice(&self, file_id: i32, events: &EventTx) {
        let failed = move |events: &EventTx| {
            let _ = events.send(AdapterEvent::VoiceFailed {
                protocol: ProtocolId::WhatsApp,
                file_id,
                stop: thinwire_protocol::VoiceStop::Unplayable,
            });
        };
        let mut state = self.lock();
        let ready = (!state.closed && state.connected)
            .then(|| state.sender.clone().zip(state.inbox.voice_media(file_id)))
            .flatten();
        let (Some((sender, media)), Ok(runtime), Ok(dir)) = (
            ready,
            tokio::runtime::Handle::try_current(),
            super::path::voice_dir(),
        ) else {
            drop(state);
            failed(events);
            return;
        };
        let gate = state
            .media_gate
            .get_or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(MEDIA_DOWNLOADS)))
            .clone();
        drop(state);
        let events = events.clone();
        runtime.spawn(async move {
            let _permit = gate.acquire_owned().await.ok();
            let stored = match sender.download(&media).await {
                Ok(bytes) => tokio::task::spawn_blocking(move || {
                    thinwire_protocol::voice_files::store(&dir, file_id, &bytes).map_err(|_| ())
                })
                .await
                .unwrap_or(Err(())),
                Err(()) => Err(()),
            };
            match stored {
                Ok(path) => {
                    let _ = events.send(AdapterEvent::VoiceReady {
                        protocol: ProtocolId::WhatsApp,
                        file_id,
                        path,
                    });
                }
                Err(()) => {
                    tracing::debug!("whatsapp audio download failed");
                    failed(&events);
                }
            }
        });
    }

    /// After a connection: the group names, then the pictures of the
    /// newest chats, one time for each link. A result of an old or a closed
    /// link is dropped.
    ///
    /// A first pairing connects before its history sync. So a history sync
    /// asks for the pictures too, when the link has not asked yet.
    fn spawn_chat_details(&self, state: &mut State, names: bool, events: &EventTx) {
        let (Some(sender), Ok(runtime)) =
            (state.sender.clone(), tokio::runtime::Handle::try_current())
        else {
            return;
        };
        let generation = state.generation;
        let avatars = if state.avatars_asked || !state.connected {
            Vec::new()
        } else {
            state.inbox.recent_chats(AVATAR_CHATS)
        };
        if !avatars.is_empty() {
            state.avatars_asked = true;
        }
        if !names && avatars.is_empty() {
            return;
        }
        let session = self.clone();
        let events = events.clone();
        runtime.spawn(async move {
            if names {
                let listed = sender.group_names().await.unwrap_or_default();
                session.finish_group_names(generation, listed, &events);
                session.mark_subjects_listed(generation);
                let pending = session.claim_unnamed(generation);
                session
                    .fetch_subjects(generation, sender.as_ref(), &pending, &events)
                    .await;
            }
            let gate = Arc::new(tokio::sync::Semaphore::new(AVATAR_LOOKUPS));
            let mut lookups = tokio::task::JoinSet::new();
            for jid in avatars {
                let sender = sender.clone();
                let gate = gate.clone();
                lookups.spawn(async move {
                    let _permit = gate.acquire_owned().await.ok();
                    let bytes = sender.profile_picture(&jid).await.ok()?;
                    let small = tokio::task::spawn_blocking(move || {
                        thinwire_protocol::photo_resize::avatar_from_bytes(&bytes)
                    })
                    .await
                    .ok()
                    .flatten()?;
                    Some((jid, small))
                });
            }
            while let Some(result) = lookups.join_next().await {
                if let Ok(Some((jid, bytes))) = result {
                    session.finish_avatar(generation, &jid, bytes, &events);
                }
            }
        });
    }

    fn finish_group_names(&self, generation: u64, names: Vec<(String, String)>, events: &EventTx) {
        let mut state = self.lock();
        if state.closed || state.generation != generation {
            return;
        }
        for (jid, name) in names {
            if let Some(row) = state.inbox.set_group_name(&jid, &name) {
                let _ = events.send(row);
            }
        }
    }

    fn mark_subjects_listed(&self, generation: u64) {
        let mut state = self.lock();
        if !state.closed && state.generation == generation {
            state.subjects_listed = true;
        }
    }

    fn take_subject_asks(state: &mut State) -> Vec<String> {
        let mut pending = Vec::new();
        for jid in state.inbox.unnamed_groups() {
            if state.subjects_asked.len() >= AVATAR_CHATS {
                break;
            }
            if state.subjects_asked.insert(jid.clone()) {
                pending.push(jid);
            }
        }
        pending
    }

    fn claim_unnamed(&self, generation: u64) -> Vec<String> {
        let mut state = self.lock();
        if state.closed || state.generation != generation || !state.subjects_listed {
            return Vec::new();
        }
        Self::take_subject_asks(&mut state)
    }

    async fn fetch_subjects(
        &self,
        generation: u64,
        sender: &dyn WhatsAppSender,
        pending: &[String],
        events: &EventTx,
    ) {
        for jid in pending {
            let Some(name) = sender.group_subject(jid).await.ok().flatten() else {
                continue;
            };
            self.finish_one_group_name(generation, jid, &name, events);
        }
    }

    fn finish_one_group_name(&self, generation: u64, jid: &str, name: &str, events: &EventTx) {
        let mut state = self.lock();
        if state.closed || state.generation != generation {
            return;
        }
        if let Some(row) = state.inbox.set_group_name(jid, name) {
            let _ = events.send(row);
        }
    }

    /// Ask the device store for the phone JID of each mentioned LID, so a
    /// mention shows the contact name and opens the one-to-one chat.
    fn spawn_phone_lookups(&self, state: &mut State, events: &EventTx) {
        if state.closed {
            return;
        }
        let Some(sender) = state.sender.clone() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let pending = state.inbox.take_phone_asks(PHONE_ASKS);
        if pending.is_empty() {
            return;
        }
        let generation = state.generation;
        let session = self.clone();
        let events = events.clone();
        runtime.spawn(async move {
            session
                .fetch_phones(generation, sender.as_ref(), &pending, &events)
                .await;
        });
    }

    async fn fetch_phones(
        &self,
        generation: u64,
        sender: &dyn WhatsAppSender,
        pending: &[String],
        events: &EventTx,
    ) {
        let mut found = Vec::new();
        for lid in pending {
            if let Some(phone) = sender.phone_of(lid).await {
                found.push((lid.clone(), phone));
            }
        }
        if found.is_empty() {
            return;
        }
        let mut state = self.lock();
        if state.closed || state.generation != generation {
            return;
        }
        let rows = state.inbox.apply_phones(found);
        if state.announced {
            for row in rows {
                let _ = events.send(row);
            }
        }
    }

    fn spawn_missing_subjects(&self, state: &mut State, events: &EventTx) {
        if !state.subjects_listed || state.closed {
            return;
        }
        let Some(sender) = state.sender.clone() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let pending = Self::take_subject_asks(state);
        if pending.is_empty() {
            return;
        }
        let generation = state.generation;
        let session = self.clone();
        let events = events.clone();
        runtime.spawn(async move {
            session
                .fetch_subjects(generation, sender.as_ref(), &pending, &events)
                .await;
        });
    }

    pub(super) fn note_older_wait(
        &self,
        generation: u64,
        request_id: String,
        jid: &str,
        before: &str,
        conversation_id: &str,
        events: &EventTx,
    ) {
        let mut state = self.lock();
        if state.closed || state.generation != generation {
            return;
        }
        let session = self.clone();
        let events = events.clone();
        let id = request_id.clone();
        let timer = tokio::runtime::Handle::current().spawn(async move {
            tokio::time::sleep(HISTORY_WAIT).await;
            session.expire_older(generation, &id, &events);
        });
        state.older_waits.insert(
            request_id,
            OlderWait {
                conversation_id: conversation_id.to_string(),
                before: before.to_string(),
                jid: jid.to_string(),
                timer: Some(timer.abort_handle()),
            },
        );
    }

    pub(super) fn expire_older(&self, generation: u64, request_id: &str, events: &EventTx) {
        let mut state = self.lock();
        if state.generation != generation {
            return;
        }
        let Some(wait) = state.older_waits.remove(request_id) else {
            return;
        };
        if let Some(timer) = wait.timer {
            timer.abort();
        }
        if state.closed || !state.announced {
            return;
        }
        tracing::debug!("whatsapp older history did not load");
        let _ = events.send(AdapterEvent::OlderHistoryLoaded {
            protocol: ProtocolId::WhatsApp,
            conversation_id: wait.conversation_id,
            before_message_id: wait.before,
            more: true,
            note: Some(OLDER_HISTORY_FAILED.to_string()),
            topic: None,
            store_cursor: None,
        });
    }

    fn clear_older_waits(state: &mut State) {
        for (_, wait) in state.older_waits.drain() {
            if let Some(timer) = wait.timer {
                timer.abort();
            }
        }
    }

    fn finish_avatar(&self, generation: u64, jid: &str, bytes: Vec<u8>, events: &EventTx) {
        let state = self.lock();
        if state.closed || state.generation != generation || bytes.is_empty() {
            return;
        }
        let _ = events.send(AdapterEvent::ChatAvatar {
            protocol: ProtocolId::WhatsApp,
            conversation_id: inbox::conversation_id(jid),
            bytes: thinwire_protocol::ImageBytes::new(bytes),
        });
    }

    /// One download ended. A result of an old or a closed link is dropped.
    fn finish_media(
        &self,
        generation: u64,
        request: &MediaRequest,
        bytes: Option<Vec<u8>>,
        events: &EventTx,
    ) {
        let mut state = self.lock();
        if state.closed || state.generation != generation {
            return;
        }
        if bytes.is_none() {
            tracing::debug!("whatsapp sticker download failed");
        }
        if let Some(row) = state
            .inbox
            .finish_media(&request.chat_jid, &request.id, bytes)
        {
            let _ = events.send(row);
        }
    }

    /// Apply one client event of link `generation` and push the UI events.
    ///
    /// The generation check, the state change, and the event sends happen
    /// under one lock. A callback of a canceled or replaced link cannot pass
    /// the check and then apply after a reset.
    #[cfg_attr(not(any(test, feature = "whatsapp-web")), allow(dead_code))]
    pub(super) fn apply(&self, event: LinkEvent, generation: u64, events: &EventTx) {
        let mut state = self.lock();
        if state.generation == NO_LINK || state.generation != generation {
            return;
        }
        let stop = event.stop_detail();
        let connected_now = matches!(event, LinkEvent::Connected);
        let history_now = matches!(event, LinkEvent::History { .. });
        let messages_now = matches!(event, LinkEvent::Messages(_));
        let mute_may_change = matches!(event, LinkEvent::History { .. } | LinkEvent::Mute { .. });
        let mut older_media = Vec::new();
        let out = match event {
            LinkEvent::Qr(code) => vec![AdapterEvent::WhatsAppQr {
                code,
                generation: state.pairing,
            }],
            LinkEvent::PairCode(code) => vec![AdapterEvent::WhatsAppPairCode {
                code,
                generation: state.pairing,
            }],
            LinkEvent::PairFailed => {
                status(events, AdapterStatus::Error, PAIR_FAILED);
                Vec::new()
            }
            LinkEvent::PairThrottled => {
                status(events, AdapterStatus::Error, PAIR_THROTTLED);
                Vec::new()
            }
            LinkEvent::QrExhausted => {
                status(events, AdapterStatus::Error, QR_EXHAUSTED);
                Vec::new()
            }
            LinkEvent::Paired => {
                status(events, AdapterStatus::Connecting, PAIRED);
                Vec::new()
            }
            LinkEvent::Connected => {
                state.connected = true;
                state.announced = true;
                status(events, AdapterStatus::Ready, CONNECTED);
                // ADR 0010 rule 1: Linked comes before the first inbox event.
                let mut out = vec![account(AccountState::Linked)];
                out.extend(state.inbox.chat_page());
                out
            }
            LinkEvent::Disconnected => {
                state.connected = false;
                // The next connection starts here: a reconnect can send
                // History before Connected (Codex r4138502149).
                state.inbox.new_connection();
                status(events, AdapterStatus::Connecting, DISCONNECTED);
                // A reconnect: the shell keeps the session and its rows.
                vec![account(AccountState::Linking)]
            }
            LinkEvent::LoggedOut => {
                let mut removed = Self::reset_state(&mut state);
                status(events, AdapterStatus::Error, LOGGED_OUT);
                // The phone removed this device: the account ended for good,
                // and the shell drops its mutes (#153). Only a logout.
                removed.push(AdapterEvent::AccountEnded {
                    protocol: ProtocolId::WhatsApp,
                });
                removed
            }
            LinkEvent::TemporaryBan => {
                state.connected = false;
                status(events, AdapterStatus::Error, TEMPORARY_BAN);
                Vec::new()
            }
            // Before Linked the rows stay in the inbox only (rule 1).
            LinkEvent::History {
                mut chats,
                push_names,
                request_id,
                ended,
            } => {
                let fetched_jid = request_id
                    .as_ref()
                    .and_then(|id| state.older_waits.get(id).map(|wait| wait.jid.clone()));
                let mut fetched_messages = Vec::new();
                if let Some(jid) = &fetched_jid {
                    for chat in &mut chats {
                        if &chat.jid == jid {
                            fetched_messages.append(&mut chat.messages);
                        }
                    }
                }
                let phone_page = fetched_jid.as_ref().map(|_| {
                    let mut messages = fetched_messages.clone();
                    messages.sort_by(|left, right| {
                        left.timestamp
                            .cmp(&right.timestamp)
                            .then_with(|| left.id.cmp(&right.id))
                    });
                    messages
                });
                let mut rows = state.inbox.apply_history(chats, push_names);
                if let Some(jid) = &fetched_jid {
                    state.inbox.apply_fetched(jid, fetched_messages);
                }
                for jid in &ended {
                    state.inbox.mark_history_ended(jid);
                }
                if !state.announced {
                    rows.clear();
                }
                if let Some(request_id) = request_id
                    && let Some(wait) = state.older_waits.remove(&request_id)
                {
                    if let Some(timer) = wait.timer {
                        timer.abort();
                    }
                    let messages = phone_page.unwrap_or_default();
                    let chat_ended = ended.iter().any(|jid| jid == &wait.jid);
                    if messages.is_empty() || chat_ended {
                        state.inbox.mark_history_ended(&wait.jid);
                    }
                    let more = !messages.is_empty() && !chat_ended;
                    if state.announced {
                        let events: Vec<_> = messages
                            .iter()
                            .map(|message| state.inbox.history_row(message))
                            .collect();
                        older_media.clone_from(&events);
                        rows.extend(events);
                        rows.push(AdapterEvent::OlderHistoryLoaded {
                            protocol: ProtocolId::WhatsApp,
                            conversation_id: wait.conversation_id,
                            before_message_id: wait.before,
                            more,
                            note: None,
                            topic: None,
                            store_cursor: None,
                        });
                    }
                }
                rows
            }
            LinkEvent::Messages(messages) => {
                let rows = state.inbox.apply_messages(messages);
                if state.announced { rows } else { Vec::new() }
            }
            LinkEvent::Reactions(reactions) => {
                let rows = state.inbox.apply_reactions(reactions);
                if state.announced { rows } else { Vec::new() }
            }
            LinkEvent::Mute { jid, mute } => {
                let row = state.inbox.apply_mute(&jid, mute);
                if state.announced {
                    row.into_iter().collect()
                } else {
                    Vec::new()
                }
            }
            LinkEvent::Contacts(contacts) => {
                let rows = state.inbox.apply_contacts(contacts);
                if state.announced { rows } else { Vec::new() }
            }
        };
        let mut out = out;
        // A history sync does not download its photos: the chat asks when
        // it opens.
        self.spawn_media(&mut state, &out, false, events);
        if !older_media.is_empty() {
            self.spawn_media(&mut state, &older_media, true, events);
        }
        if connected_now || history_now {
            self.spawn_chat_details(&mut state, connected_now, events);
        }
        if history_now || messages_now {
            self.spawn_missing_subjects(&mut state, events);
        }
        if connected_now || history_now || messages_now {
            self.spawn_phone_lookups(&mut state, events);
        }
        if let Some(detail) = stop {
            state.connected = false;
            state.stopped = Some(detail);
            // Every terminal event ends the link here. The adapter's shutdown
            // stops the timer through the same helper (Codex r4139171813).
            Self::abort_mute_timer(&mut state);
            // Logout, ban, or a dead pairing ends the session.
            out.push(account(AccountState::Unlinked));
        }
        for event in out {
            let _ = events.send(event);
        }
        if mute_may_change {
            self.arm_mute_timer(&mut state, generation, events);
        }
    }

    /// Wake at the next timed mute end, so the row stops being muted then
    /// and not only at the next message (#168 item 13). An earlier end
    /// replaces the timer.
    fn arm_mute_timer(&self, state: &mut State, generation: u64, events: &EventTx) {
        let Some(end) = state.inbox.next_mute_end() else {
            // Off or Forever: no timed mute remains. The timer of the old
            // end would keep the session until that end (Codex r4139365614).
            Self::abort_mute_timer(state);
            return;
        };
        if let Some(timer) = &state.mute_timer {
            if timer.end <= end && !timer.task.is_finished() {
                return;
            }
            timer.task.abort();
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        state.mute_timers += 1;
        let id = state.mute_timers;
        let wait = u64::try_from(end.saturating_sub(inbox::now_ms())).unwrap_or(0);
        let session = self.clone();
        let events = events.clone();
        let task = runtime.spawn(async move {
            // One millisecond more: the sleep clock and the wall clock can
            // differ. A timer that wakes too early arms again.
            tokio::time::sleep(Duration::from_millis(wait.saturating_add(1))).await;
            session.end_mutes(generation, id, &events);
        });
        state.mute_timer = Some(MuteTimer { end, id, task });
    }

    /// Mute timer `id` of link `generation` fired: end the mutes that
    /// passed. A timer that was stopped or replaced does nothing.
    fn end_mutes(&self, generation: u64, id: u64, events: &EventTx) {
        let mut state = self.lock();
        let armed = state
            .mute_timer
            .as_ref()
            .is_some_and(|timer| timer.id == id);
        if !armed || state.generation == NO_LINK || state.generation != generation {
            return;
        }
        state.mute_timer = None;
        let rows = state.inbox.expire_mutes(inbox::now_ms());
        if state.announced {
            for row in rows {
                let _ = events.send(row);
            }
        }
        self.arm_mute_timer(&mut state, generation, events);
    }

    /// Apply the result of a send that started on link `generation`, and
    /// answer the shell's request only now, after the network result
    /// (qaproto, #98): `SendAccepted` when the server accepted the message,
    /// else `SendRejected`. A failed new send loses its row (the text stays
    /// in the compose field); a failed retry keeps its row as Failed.
    ///
    /// A result from an older link is dropped: that link's inbox and sender
    /// are gone, and an `Unlinked` result must not reset the new link.
    ///
    /// Returns `true` if the phone revoked the current link. The caller must
    /// tell the link owner, so it stops the client and deletes the store.
    #[must_use]
    pub(super) fn finish_send(
        &self,
        generation: u64,
        jid: &str,
        pending: &str,
        result: Result<String, SendFailure>,
        answer: &SendRequest,
        events: &EventTx,
    ) -> bool {
        let mut state = self.lock();
        if state.closed {
            // `Stopped` is out: the core is past this adapter. Its exit log
            // names the send as abandoned.
            tracing::warn!(
                chat = %thinwire_protocol::LogChatId(&answer.conversation_id),
                "whatsapp send ended after shutdown; its result is dropped"
            );
            return false;
        }
        if state.generation != generation {
            // The link that sent it is gone, and its rows with it. Rule 4:
            // answer also a request that a reconnect lost.
            let _ = events.send(answer.rejected());
            return false;
        }
        let revoked = result == Err(SendFailure::Unlinked);
        let out = match result {
            Ok(server_id) => {
                let mut out = vec![answer.accepted()];
                out.extend(state.inbox.confirm_send(jid, pending, server_id));
                out
            }
            Err(failure) => {
                status(events, AdapterStatus::Error, failure.detail());
                let mut out = if answer.retry {
                    state.inbox.fail_send(jid, pending)
                } else {
                    state.inbox.drop_send(jid, pending)
                };
                out.push(answer.rejected());
                if failure == SendFailure::Unlinked {
                    out.extend(Self::reset_state(&mut state));
                    out.push(account(AccountState::Unlinked));
                }
                out
            }
        };
        for event in out {
            let _ = events.send(event);
        }
        revoked
    }
}

/// The shell's request of one send or retry (ADR 0010 rule 4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SendRequest {
    pub conversation_id: String,
    pub request: u64,
    /// A `ResendMessage` of a failed row, not a new `SendText`.
    pub retry: bool,
}

impl SendRequest {
    fn accepted(&self) -> AdapterEvent {
        AdapterEvent::SendAccepted {
            protocol: ProtocolId::WhatsApp,
            conversation_id: self.conversation_id.clone(),
            request: self.request,
        }
    }

    fn rejected(&self) -> AdapterEvent {
        AdapterEvent::SendRejected {
            protocol: ProtocolId::WhatsApp,
            conversation_id: self.conversation_id.clone(),
            request: self.request,
        }
    }
}

/// The link state of the WhatsApp account. Only this event changes it in the
/// shell (ADR 0010 rule 1); a `Status` never does.
pub(super) const fn account(state: AccountState) -> AdapterEvent {
    AdapterEvent::Account {
        protocol: ProtocolId::WhatsApp,
        state,
    }
}

fn status(events: &EventTx, status: AdapterStatus, detail: &str) {
    emit_status(events, ProtocolId::WhatsApp, status, detail);
}

#[cfg(test)]
pub(super) mod fake {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{
        ActionFuture, DownloadFuture, HistoryAsk, MediaRef, NamesFuture, PhoneFuture, SendFailure,
        SendFuture, SubjectFuture, TargetRef, WhatsAppSender,
    };

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(in crate::whatsapp) struct RecordedHistory {
        pub chat_jid: String,
        pub oldest_id: String,
        pub from_me: bool,
        pub timestamp_ms: i64,
        pub count: i32,
        pub request_id: String,
    }

    /// Records sends. Returns `SRV<n>` ids, or the failure in `fail`. A
    /// download returns `media` when set, else it fails.
    #[derive(Default)]
    pub(in crate::whatsapp) struct FakeSender {
        pub sent: Mutex<Vec<(String, String)>>,
        pub fail: Option<SendFailure>,
        pub media: Option<Vec<u8>>,
        pub downloads: Mutex<u32>,
        /// The group subjects of `group_names`.
        pub names: Vec<(String, String)>,
        pub subjects: Vec<(String, String)>,
        pub subject_asks: Mutex<Vec<String>>,
        pub history_seq: AtomicU64,
        pub history_asks: Mutex<Vec<RecordedHistory>>,
        /// The bytes of each `profile_picture`. `None` has no picture.
        pub picture: Option<Vec<u8>>,
        /// Each reply, reaction, and delete, as "kind id".
        pub actions: Mutex<Vec<String>>,
        /// The phone JID of each LID that the store knows.
        pub phones: Vec<(String, String)>,
        pub phone_asks: Mutex<Vec<String>>,
    }

    impl FakeSender {
        fn act(&self, action: String) -> ActionFuture<'_> {
            self.actions.lock().expect("fake lock").push(action);
            Box::pin(async { Ok(()) })
        }
    }

    impl WhatsAppSender for FakeSender {
        fn send_text<'a>(&'a self, chat_jid: &'a str, body: &'a str) -> SendFuture<'a> {
            Box::pin(async move {
                let mut sent = self.sent.lock().expect("fake lock");
                sent.push((chat_jid.to_string(), body.to_string()));
                if let Some(failure) = self.fail {
                    Err(failure)
                } else {
                    Ok(format!("SRV{}", sent.len()))
                }
            })
        }

        fn download<'a>(&'a self, _media: &'a MediaRef) -> DownloadFuture<'a> {
            Box::pin(async move {
                *self.downloads.lock().expect("fake lock") += 1;
                self.media.clone().ok_or(())
            })
        }

        fn group_names(&self) -> NamesFuture<'_> {
            Box::pin(async move { Ok(self.names.clone()) })
        }

        fn group_subject<'a>(&'a self, jid: &'a str) -> SubjectFuture<'a> {
            Box::pin(async move {
                self.subject_asks
                    .lock()
                    .expect("fake lock")
                    .push(jid.to_string());
                Ok(self
                    .subjects
                    .iter()
                    .find(|(id, _)| id == jid)
                    .map(|(_, name)| name.clone()))
            })
        }

        fn phone_of<'a>(&'a self, lid: &'a str) -> PhoneFuture<'a> {
            Box::pin(async move {
                self.phone_asks
                    .lock()
                    .expect("fake lock")
                    .push(lid.to_string());
                self.phones
                    .iter()
                    .find(|(id, _)| id == lid)
                    .map(|(_, phone)| phone.clone())
            })
        }

        fn allocate_history_id(&self) -> Option<String> {
            let n = self.history_seq.fetch_add(1, Ordering::Relaxed) + 1;
            Some(format!("hist-{n}"))
        }

        fn request_history<'a>(&'a self, ask: &'a HistoryAsk<'a>) -> ActionFuture<'a> {
            self.history_asks
                .lock()
                .expect("fake lock")
                .push(RecordedHistory {
                    chat_jid: ask.chat_jid.to_string(),
                    oldest_id: ask.oldest_id.to_string(),
                    from_me: ask.oldest_from_me,
                    timestamp_ms: ask.oldest_timestamp_ms,
                    count: ask.count,
                    request_id: ask.request_id.to_string(),
                });
            Box::pin(async { Ok(()) })
        }

        fn profile_picture<'a>(&'a self, _jid: &'a str) -> DownloadFuture<'a> {
            Box::pin(async move { self.picture.clone().ok_or(()) })
        }

        fn send_reply<'a>(
            &'a self,
            chat_jid: &'a str,
            body: &'a str,
            quote: &'a TargetRef,
        ) -> SendFuture<'a> {
            self.actions
                .lock()
                .expect("fake lock")
                .push(format!("reply {}", quote.id));
            self.send_text(chat_jid, body)
        }

        fn react<'a>(
            &'a self,
            _chat_jid: &'a str,
            target: &'a TargetRef,
            emoji: &'a str,
        ) -> ActionFuture<'a> {
            self.act(format!("react {} {emoji}", target.id))
        }

        fn revoke<'a>(&'a self, _chat_jid: &'a str, target: &'a TargetRef) -> ActionFuture<'a> {
            self.act(format!("revoke {}", target.id))
        }

        fn delete_for_me<'a>(
            &'a self,
            _chat_jid: &'a str,
            target: &'a TargetRef,
        ) -> ActionFuture<'a> {
            self.act(format!("delete_for_me {}", target.id))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    use std::sync::Arc;

    use super::*;
    use crate::whatsapp::inbox::tests::message;
    use crate::whatsapp::inbox::{HistoryChat, Mute};
    use thinwire_protocol::Conversation;

    const CHAT: &str = "111@s.whatsapp.net";

    fn linked() -> (Session, EventTx, UnboundedReceiver<AdapterEvent>) {
        let session = Session::default();
        session.begin(1);
        let (tx, rx) = unbounded_channel();
        session.apply(LinkEvent::Connected, 1, &tx);
        (session, tx, rx)
    }

    fn history(mute: Mute) -> LinkEvent {
        LinkEvent::History {
            chats: vec![HistoryChat {
                jid: CHAT.into(),
                name: None,
                unread: 0,
                timestamp: 10,
                messages: Vec::new(),
                mute,
            }],
            push_names: Vec::new(),
            request_id: None,
            ended: Vec::new(),
        }
    }

    fn last_row(rx: &mut UnboundedReceiver<AdapterEvent>) -> Option<Conversation> {
        let mut row = None;
        while let Ok(event) = rx.try_recv() {
            if let AdapterEvent::ConversationUpsert { conversation } = event {
                row = Some(conversation);
            }
        }
        row
    }

    fn armed_end(session: &Session) -> Option<i64> {
        session.lock().mute_timer.as_ref().map(|timer| timer.end)
    }

    /// A live sticker shows "[sticker]", then the row comes again with its
    /// image. The download runs one time.
    #[tokio::test]
    async fn a_live_sticker_row_comes_again_with_its_image() {
        let (session, tx, mut rx) = linked();
        let sender = Arc::new(fake::FakeSender {
            media: Some(vec![9, 9, 9]),
            ..Default::default()
        });
        assert!(session.attach_sender(1, sender.clone()));
        let sticker = crate::whatsapp::inbox::tests::sticker_message(CHAT, "s1", 5);
        session.apply(LinkEvent::Messages(vec![sticker]), 1, &tx);
        let mut rows = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while rows.len() < 2 && tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
                Ok(Some(AdapterEvent::MessageReceived { message })) if message.id == "s1" => {
                    rows.push(message);
                }
                Ok(Some(_)) | Err(_) => {}
                Ok(None) => break,
            }
        }
        assert_eq!(rows.len(), 2, "the row and the row with its image");
        assert_eq!(rows[0].body, "[sticker]");
        assert!(rows[0].photo.is_none());
        assert_eq!(rows[1].body, "");
        assert!(rows[1].photo.is_some());
        assert_eq!(*sender.downloads.lock().expect("lock"), 1);

        // A close drops a download that ends later.
        session.close();
        let late = crate::whatsapp::inbox::tests::sticker_message(CHAT, "s2", 6);
        session.apply(LinkEvent::Messages(vec![late]), 1, &tx);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(*sender.downloads.lock().expect("lock"), 1);
    }

    /// A voice note row has audio with the handle of its message. Play
    /// downloads it into the voice folder. A handle with no message fails.
    #[tokio::test]
    async fn a_voice_note_downloads_into_the_voice_folder() {
        let (session, tx, mut rx) = linked();
        let sender = Arc::new(fake::FakeSender {
            media: Some(b"OggS-voice".to_vec()),
            ..Default::default()
        });
        assert!(session.attach_sender(1, sender.clone()));
        let voice = WaMessage {
            voice: Some(crate::whatsapp::inbox::Voice {
                seconds: 7,
                media: MediaRef::new("audio".to_string()),
            }),
            ..message(CHAT, "v1", "[voice message]", 5)
        };
        session.apply(LinkEvent::Messages(vec![voice]), 1, &tx);
        let mut audio = None;
        while let Ok(event) = rx.try_recv() {
            if let AdapterEvent::MessageReceived { message } = event
                && message.id == "v1"
            {
                audio = message.audio;
            }
        }
        let audio = audio.expect("the row has audio");
        assert_eq!(audio.duration_secs(), 7);
        let file_id = audio.file_id();
        assert_eq!(file_id, crate::whatsapp::inbox::voice_file_id(CHAT, "v1"));
        session.download_voice(file_id, &tx);
        let ready = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match rx.recv().await {
                    Some(AdapterEvent::VoiceReady {
                        file_id: id, path, ..
                    }) if id == file_id => {
                        return Some(path);
                    }
                    Some(_) => {}
                    None => return None,
                }
            }
        })
        .await
        .expect("an answer")
        .expect("ready");
        assert_eq!(std::fs::read(ready.as_path()).expect("file"), b"OggS-voice");
        session.download_voice(file_id.wrapping_add(1).max(1), &tx);
        let failed = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("an answer");
        assert!(matches!(failed, Some(AdapterEvent::VoiceFailed { .. })));
        let _ = std::fs::remove_file(ready.as_path());
    }

    /// A connection asks for the group subjects and the chat pictures. A
    /// group with no name gets its subject, and the chat gets a small JPEG
    /// picture. A reconnect of the same link does not ask for the pictures
    /// again.
    #[tokio::test]
    async fn a_connection_names_groups_and_fetches_chat_pictures() {
        const GROUP: &str = "120363000000000001@g.us";
        let (session, tx, mut rx) = linked();
        let sender = Arc::new(fake::FakeSender {
            names: vec![(GROUP.into(), "Lake trip".into())],
            picture: Some(crate::whatsapp::inbox::tests::PIXEL_PNG.to_vec()),
            ..Default::default()
        });
        assert!(session.attach_sender(1, sender.clone()));
        session.apply(LinkEvent::Connected, 1, &tx);
        session.apply(
            LinkEvent::Messages(vec![message(GROUP, "g1", "hi all", 5)]),
            1,
            &tx,
        );
        session.apply(LinkEvent::Disconnected, 1, &tx);
        session.apply(LinkEvent::Connected, 1, &tx);
        let mut named = false;
        let mut pictures = 0;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline && !(named && pictures > 0) {
            match tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
                Ok(Some(AdapterEvent::ConversationUpsert { conversation }))
                    if conversation.title == "Lake trip" =>
                {
                    named = true;
                }
                Ok(Some(AdapterEvent::ChatAvatar {
                    bytes,
                    conversation_id,
                    ..
                })) => {
                    assert_eq!(conversation_id, inbox::conversation_id(GROUP));
                    assert!(bytes.as_slice().starts_with(&[0xFF, 0xD8]), "a JPEG");
                    pictures += 1;
                }
                Ok(Some(_)) | Err(_) => {}
                Ok(None) => break,
            }
        }
        assert!(named, "the group got its subject");
        assert_eq!(pictures, 1, "one picture for the one chat, one time");
    }

    /// A mentioned LID asks the device store for its phone JID. The open
    /// chat then shows the contact name of the mention.
    #[tokio::test]
    async fn a_mentioned_lid_gets_its_phone_from_the_store() {
        const GROUP: &str = "120363000000000010@g.us";
        const LID: &str = "223853863252019@lid";
        const PHONE: &str = "5511999990000@s.whatsapp.net";
        let session = Session::default();
        session.begin(1);
        let (tx, mut rx) = unbounded_channel();
        let sender = Arc::new(fake::FakeSender {
            phones: vec![(LID.into(), PHONE.into())],
            ..Default::default()
        });
        assert!(session.attach_sender(1, sender.clone()));
        session.apply(
            LinkEvent::History {
                chats: vec![HistoryChat {
                    jid: PHONE.into(),
                    name: Some("Ana".into()),
                    unread: 0,
                    timestamp: 5,
                    messages: vec![message(PHONE, "d1", "oi", 5)],
                    mute: Mute::Off,
                }],
                push_names: Vec::new(),
                request_id: None,
                ended: Vec::new(),
            },
            1,
            &tx,
        );
        session.apply(LinkEvent::Connected, 1, &tx);
        let _ = session.with_inbox(|inbox| inbox.view(Some(GROUP)));
        let mut mention = message(GROUP, "m1", "@223853863252019 hi", 10);
        mention.mentions = vec![LID.into()];
        session.apply(LinkEvent::Messages(vec![mention]), 1, &tx);
        let mut named = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline && !named {
            match tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
                Ok(Some(AdapterEvent::MessageReceived { message }))
                    if message.body == "@Ana hi" =>
                {
                    assert_eq!(
                        message.links[0].chat(),
                        Some(inbox::conversation_id(PHONE).as_str())
                    );
                    named = true;
                }
                Ok(Some(_)) | Err(_) => {}
                Ok(None) => break,
            }
        }
        assert!(named, "the open chat shows the contact name");
        let asks = sender.phone_asks.lock().expect("fake lock").clone();
        assert_eq!(asks, vec![LID.to_string()]);
    }

    #[tokio::test]
    async fn a_group_with_no_stored_name_gets_its_subject() {
        const UNNAMED: &str = "120363000000000001@g.us";
        const NAMED: &str = "120363000000000002@g.us";
        const LATER: &str = "120363000000000003@g.us";
        let session = Session::default();
        session.begin(1);
        let (tx, mut rx) = unbounded_channel();
        let sender = Arc::new(fake::FakeSender {
            subjects: vec![
                (UNNAMED.into(), "School run".into()),
                (NAMED.into(), "Should not replace".into()),
                (LATER.into(), "Parents".into()),
            ],
            ..Default::default()
        });
        assert!(session.attach_sender(1, sender.clone()));
        let chat = |jid: &str, name: Option<&str>, body: &str| HistoryChat {
            jid: jid.into(),
            name: name.map(str::to_string),
            unread: 0,
            timestamp: 10,
            messages: vec![message(jid, "m1", body, 10)],
            mute: Mute::Off,
        };
        session.apply(
            LinkEvent::History {
                chats: vec![
                    chat(UNNAMED, None, "Guten Morgen"),
                    chat(NAMED, Some("Family"), "hi"),
                ],
                push_names: Vec::new(),
                request_id: None,
                ended: Vec::new(),
            },
            1,
            &tx,
        );
        session.apply(LinkEvent::Connected, 1, &tx);
        let mut saw_school = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline && !saw_school {
            match tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
                Ok(Some(AdapterEvent::ConversationUpsert { conversation }))
                    if conversation.title == "School run" =>
                {
                    saw_school = true;
                }
                Ok(Some(AdapterEvent::ConversationUpsert { conversation }))
                    if conversation.id == inbox::conversation_id(NAMED) =>
                {
                    assert_eq!(conversation.title, "Family");
                }
                Ok(Some(_)) | Err(_) => {}
                Ok(None) => break,
            }
        }
        assert!(
            saw_school,
            "the unnamed group gets the subject from metadata"
        );
        let asks = sender.subject_asks.lock().expect("fake lock").clone();
        assert_eq!(
            asks,
            vec![UNNAMED.to_string()],
            "a named group is left alone"
        );
        session.apply(
            LinkEvent::Messages(vec![message(LATER, "m2", "hello", 11)]),
            1,
            &tx,
        );
        let mut saw_parents = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline && !saw_parents {
            match tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
                Ok(Some(AdapterEvent::ConversationUpsert { conversation }))
                    if conversation.title == "Parents" =>
                {
                    saw_parents = true;
                }
                Ok(Some(_)) | Err(_) => {}
                Ok(None) => break,
            }
        }
        assert!(saw_parents, "a group that appears with no name is fetched");
        let asks = sender.subject_asks.lock().expect("fake lock").clone();
        assert!(asks.contains(&LATER.to_string()));
        assert!(!asks.contains(&NAMED.to_string()));
    }

    #[tokio::test]
    async fn a_link_asks_for_at_most_fifty_unnamed_subjects() {
        let session = Session::default();
        session.begin(1);
        let (tx, _rx) = unbounded_channel();
        let sender = Arc::new(fake::FakeSender::default());
        assert!(session.attach_sender(1, sender.clone()));
        let total = AVATAR_CHATS + 10;
        let chats: Vec<HistoryChat> = (0..total)
            .map(|n| {
                let jid = format!("120363{n:012}@g.us");
                HistoryChat {
                    jid: jid.clone(),
                    name: None,
                    unread: 0,
                    timestamp: 10,
                    messages: vec![message(&jid, "m1", "hi", 10)],
                    mute: Mute::Off,
                }
            })
            .collect();
        session.apply(
            LinkEvent::History {
                chats,
                push_names: Vec::new(),
                request_id: None,
                ended: Vec::new(),
            },
            1,
            &tx,
        );
        session.apply(LinkEvent::Connected, 1, &tx);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline
            && sender.subject_asks.lock().expect("fake lock").len() < AVATAR_CHATS
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            sender.subject_asks.lock().expect("fake lock").len(),
            AVATAR_CHATS,
            "the connect pass stopped before the cap"
        );
        session.apply(
            LinkEvent::Messages(vec![message("120363000000000000@g.us", "m2", "again", 11)]),
            1,
            &tx,
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        let asks = sender.subject_asks.lock().expect("fake lock").len();
        assert_eq!(
            asks, AVATAR_CHATS,
            "a later message asked for {asks} subjects in one run"
        );
    }

    /// #168 item 12: the session tells the inbox about each connection. A
    /// history value after a reconnect refreshes a mute that a live change
    /// of the last connection set.
    #[test]
    fn a_reconnect_lets_history_refresh_a_live_mute() {
        let (session, tx, mut rx) = linked();
        let live = LinkEvent::Mute {
            jid: CHAT.into(),
            mute: Mute::Forever,
        };
        session.apply(live, 1, &tx);
        session.apply(history(Mute::Off), 1, &tx);
        assert!(
            last_row(&mut rx).expect("row").muted,
            "the live change wins"
        );

        session.apply(LinkEvent::Disconnected, 1, &tx);
        session.apply(LinkEvent::Connected, 1, &tx);
        session.apply(history(Mute::Off), 1, &tx);
        assert!(
            !last_row(&mut rx).expect("row").muted,
            "unmuted while offline"
        );
    }

    /// Codex r4138502149: a reconnect can send History before Connected.
    /// That history value still applies over a live change of the last
    /// connection.
    #[test]
    fn reconnect_history_before_connected_applies_its_mute() {
        let (session, tx, mut rx) = linked();
        let live = LinkEvent::Mute {
            jid: CHAT.into(),
            mute: Mute::Forever,
        };
        session.apply(live, 1, &tx);
        session.apply(history(Mute::Off), 1, &tx);
        assert!(
            last_row(&mut rx).expect("row").muted,
            "the live change wins"
        );

        session.apply(LinkEvent::Disconnected, 1, &tx);
        session.apply(history(Mute::Off), 1, &tx);
        assert!(!last_row(&mut rx).expect("row").muted, "before Connected");
        session.apply(LinkEvent::Connected, 1, &tx);
        assert!(
            !last_row(&mut rx).expect("row").muted,
            "the chat page agrees"
        );
    }

    /// Codex r4138502149: a new link is a new connection too. Its history
    /// before Connected applies over a live change of the old link.
    #[test]
    fn a_new_link_lets_history_refresh_a_live_mute() {
        let (session, tx, mut rx) = linked();
        let live = LinkEvent::Mute {
            jid: CHAT.into(),
            mute: Mute::Forever,
        };
        session.apply(live, 1, &tx);
        assert!(last_row(&mut rx).expect("row").muted);

        session.begin(2);
        session.apply(history(Mute::Off), 2, &tx);
        session.apply(LinkEvent::Connected, 2, &tx);
        assert!(!last_row(&mut rx).expect("row").muted);
    }

    /// Codex r4138502157: a new link drops the mute timer of the old link.
    /// A mute of the new link that ends later gets its own timer.
    #[tokio::test]
    async fn a_new_link_drops_the_old_mute_timer() {
        let (session, tx, _rx) = linked();
        let now = inbox::now_ms();
        let mute = |end: i64| LinkEvent::Mute {
            jid: CHAT.into(),
            mute: Mute::UntilMs(end),
        };
        session.apply(mute(now + 60_000), 1, &tx);
        assert_eq!(armed_end(&session), Some(now + 60_000));

        session.begin(2);
        assert_eq!(armed_end(&session), None, "the old timer is gone");
        session.apply(LinkEvent::Connected, 2, &tx);
        session.apply(mute(now + 120_000), 2, &tx);
        assert_eq!(
            armed_end(&session),
            Some(now + 120_000),
            "a timer of link 2"
        );
    }

    /// #168 item 13: at the mute end the row stops being muted, with no new
    /// message. The window title counts its unread again.
    #[tokio::test]
    async fn the_mute_timer_unmutes_the_row_at_its_end() {
        let (session, tx, mut rx) = linked();
        session.apply(
            LinkEvent::Messages(vec![message(CHAT, "a", "one", 1)]),
            1,
            &tx,
        );
        let end = inbox::now_ms() + 100;
        let timed = LinkEvent::Mute {
            jid: CHAT.into(),
            mute: Mute::UntilMs(end),
        };
        session.apply(timed, 1, &tx);
        assert!(last_row(&mut rx).expect("row").muted);

        let row = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let AdapterEvent::ConversationUpsert { conversation } =
                    rx.recv().await.expect("the session is alive")
                {
                    return conversation;
                }
            }
        })
        .await
        .expect("the timer sends the row");
        assert!(!row.muted);
        assert_eq!(row.unread, 1, "its unread counts in the title again");
        assert!(inbox::now_ms() >= end);
    }

    /// Codex r4139171813: every terminal link event stops a pending mute
    /// timer, not only a logout. No event comes after `Unlinked`, and the
    /// timer task lets go of the session.
    #[tokio::test]
    async fn every_terminal_event_stops_the_mute_timer() {
        for terminal in [
            LinkEvent::TemporaryBan,
            LinkEvent::LoggedOut,
            LinkEvent::PairFailed,
            LinkEvent::PairThrottled,
            LinkEvent::QrExhausted,
        ] {
            let name = format!("{terminal:?}");
            let is_ban = matches!(terminal, LinkEvent::TemporaryBan);
            let (session, tx, mut rx) = linked();
            let owners = session.owners();
            let end = inbox::now_ms() + 100;
            let timed = LinkEvent::Mute {
                jid: CHAT.into(),
                mute: Mute::UntilMs(end),
            };
            session.apply(timed, 1, &tx);
            assert!(session.has_mute_timer(), "{name}");
            assert!(terminal.stops_link(), "{name}");

            session.apply(terminal, 1, &tx);
            assert!(!session.has_mute_timer(), "{name}: the timer stopped");
            let mut last = None;
            while let Ok(event) = rx.try_recv() {
                last = Some(event);
            }
            assert!(
                matches!(
                    last,
                    Some(AdapterEvent::Account {
                        state: AccountState::Unlinked,
                        ..
                    })
                ),
                "{name}: Unlinked is the last event"
            );
            if is_ban {
                // Past the mute end: the stopped timer sends nothing.
                tokio::time::sleep(Duration::from_millis(300)).await;
                assert!(inbox::now_ms() > end);
                assert!(rx.try_recv().is_err(), "no event after Unlinked");
            }
            for _ in 0..100 {
                if session.owners() <= owners {
                    break;
                }
                tokio::task::yield_now().await;
            }
            assert!(session.owners() <= owners, "{name}: the task let go");
        }
    }

    /// Codex r4139365614: Off or Forever leaves no timed mute. The timer
    /// that waited for the old end stops, and the task lets go of the
    /// session. A later timed mute gets its own timer.
    #[tokio::test]
    async fn off_or_forever_stops_the_mute_timer() {
        let cases = [
            ("live off", false, Mute::Off),
            ("live forever", false, Mute::Forever),
            ("history off", true, Mute::Off),
            ("history forever", true, Mute::Forever),
        ];
        for (name, from_history, mute) in cases {
            let (session, tx, _rx) = linked();
            let owners = session.owners();
            let end = inbox::now_ms() + 60_000;
            session.apply(
                LinkEvent::Mute {
                    jid: CHAT.into(),
                    mute: Mute::UntilMs(end),
                },
                1,
                &tx,
            );
            assert!(session.has_mute_timer(), "{name}");
            if from_history {
                session.apply(LinkEvent::Disconnected, 1, &tx);
                session.apply(history(mute), 1, &tx);
            } else {
                session.apply(
                    LinkEvent::Mute {
                        jid: CHAT.into(),
                        mute,
                    },
                    1,
                    &tx,
                );
            }
            assert!(!session.has_mute_timer(), "{name}");
            for _ in 0..100 {
                if session.owners() <= owners {
                    break;
                }
                tokio::task::yield_now().await;
            }
            assert!(session.owners() <= owners, "{name}: the task let go");

            let again = inbox::now_ms() + 120_000;
            session.apply(
                LinkEvent::Mute {
                    jid: CHAT.into(),
                    mute: Mute::UntilMs(again),
                },
                1,
                &tx,
            );
            assert_eq!(armed_end(&session), Some(again), "{name}: a new timer");
        }
    }

    /// #238 (Codex r4216328520): a send result that comes after `close`
    /// publishes nothing: no `SendAccepted`, no `SendRejected`, no row.
    #[test]
    fn a_send_result_after_close_publishes_nothing() {
        let (session, tx, mut rx) = linked();
        let answer = SendRequest {
            conversation_id: format!("whatsapp:{CHAT}"),
            request: 7,
            retry: false,
        };
        // Before the close a result answers its request.
        assert!(!session.finish_send(1, CHAT, "local-1", Ok("server-1".into()), &answer, &tx));
        let before: Vec<AdapterEvent> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(
            before
                .iter()
                .any(|event| matches!(event, AdapterEvent::SendAccepted { request: 7, .. })),
            "{before:?}"
        );

        session.close();
        for result in [
            Ok("server-2".to_string()),
            Err(SendFailure::Network),
            Err(SendFailure::Unlinked),
        ] {
            let revoked = session.finish_send(1, CHAT, "local-2", result, &answer, &tx);
            assert!(!revoked, "a sealed send never reports a revoke");
        }
        // A stale generation answers with SendRejected while open; once
        // closed it publishes nothing either.
        assert!(!session.finish_send(9, CHAT, "local-3", Err(SendFailure::Rejected), &answer, &tx));
        assert!(rx.try_recv().is_err(), "nothing after close");
        assert!(
            session.is_connected(),
            "a sealed Unlinked result resets nothing"
        );
    }

    /// Codex r4139029620: `abort` cannot stop a timer task that already
    /// waits for the lock. The task of a stopped timer does nothing when it
    /// runs late.
    #[tokio::test]
    async fn a_stopped_mute_timer_task_does_nothing() {
        let (session, tx, mut rx) = linked();
        let passed = LinkEvent::Mute {
            jid: CHAT.into(),
            mute: Mute::UntilMs(inbox::now_ms() - 1),
        };
        // No await from here on: the spawned timer task cannot run.
        session.apply(passed, 1, &tx);
        last_row(&mut rx);
        let id = session
            .lock()
            .mute_timer
            .as_ref()
            .map(|timer| timer.id)
            .expect("armed");
        session.close();
        session.end_mutes(1, id, &tx);
        assert!(last_row(&mut rx).is_none(), "no row from a stopped timer");
    }

    /// #168 item 13: one timer waits for the earliest end. A reset stops it.
    #[tokio::test]
    async fn the_mute_timer_waits_for_the_earliest_end_and_stops_on_reset() {
        let (session, tx, _rx) = linked();
        let now = inbox::now_ms();
        let mute = |jid: &str, end: i64| LinkEvent::Mute {
            jid: jid.into(),
            mute: Mute::UntilMs(end),
        };
        session.apply(mute(CHAT, now + 120_000), 1, &tx);
        assert_eq!(armed_end(&session), Some(now + 120_000));
        session.apply(mute("222@s.whatsapp.net", now + 60_000), 1, &tx);
        assert_eq!(armed_end(&session), Some(now + 60_000), "the earlier end");
        session.apply(mute("333@s.whatsapp.net", now + 90_000), 1, &tx);
        assert_eq!(armed_end(&session), Some(now + 60_000), "a later end waits");

        session.reset();
        assert_eq!(armed_end(&session), None);
    }
}
