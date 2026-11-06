// SPDX-License-Identifier: AGPL-3.0-only
//! WhatsApp chat list and message store. No whatsapp-rust types and no UI calls.
//!
//! The live client (feature `whatsapp-web`) maps history-sync chunks and live
//! message batches into these records on the worker. Default builds test the
//! same rules with fakes.

use std::any::Any;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::sync::Arc;

use thinwire_protocol::{
    AdapterEvent, Arrival, ChatMessage, Conversation, Delivery, ImageBytes, MessageAudio,
    MessagePhoto, PhotoSize, ProtocolId,
};

/// Prefix on every WhatsApp conversation id. The rest is the chat JID.
pub(super) const CONVERSATION_PREFIX: &str = "whatsapp:";

/// Messages kept per chat. Older ones drop off the front.
pub(super) const MESSAGES_PER_CHAT: usize = 200;

/// Handled ids kept per chat. Past this, the older half goes, oldest first
/// (#158 review).
const HANDLED_PER_CHAT: usize = 1_000;

/// Messages sent to the UI when a chat opens.
pub(super) const OPEN_CHAT_MESSAGES: usize = 50;

/// Chats sent to the UI on one chat-list load.
pub(super) const CHAT_PAGE: usize = 200;

const PREVIEW_CHARS: usize = 80;
const OUTBOUND_SENDER: &str = "You";
const GROUP_SERVER: &str = "@g.us";
const PENDING_PREFIX: &str = "pending:";
const STATUS_BROADCAST: &str = "status@broadcast";
const BROADCAST_SERVER: &str = "@broadcast";
const NEWSLETTER_SERVER: &str = "@newsletter";

/// Downloaded sticker images kept in memory. Past this, the oldest goes.
/// A chat that opens again downloads a dropped image again.
const MEDIA_CACHE: usize = 200;

/// The largest sticker image the helper keeps. It must fit in one wire
/// line: it is `thinwire_ipc::WirePhoto::MAX_BYTES` (a test checks it).
pub(super) const MEDIA_MAX_BYTES: usize = 512 * 1024;

/// The body of a photo message with no caption.
pub(super) const PHOTO_LABEL: &str = "[photo]";

/// The longest side of a sticker in the thread, in points. The image keeps
/// its own pixels, so it stays sharp on a high-density screen.
const STICKER_SIDE: u32 = 160;

/// The download reference of one media file. Only the live client can read
/// it: it holds the whatsapp-rust message. `Debug` shows no key or path.
#[derive(Clone)]
pub(super) struct MediaRef(Arc<dyn Any + Send + Sync>);

impl MediaRef {
    #[cfg_attr(not(any(test, feature = "whatsapp-web")), allow(dead_code))]
    pub(super) fn new(value: impl Any + Send + Sync) -> Self {
        Self(Arc::new(value))
    }

    #[cfg_attr(not(feature = "whatsapp-web"), allow(dead_code))]
    pub(super) fn get<T: Any>(&self) -> Option<&T> {
        self.0.downcast_ref()
    }
}

impl fmt::Debug for MediaRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MediaRef")
    }
}

impl PartialEq for MediaRef {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for MediaRef {}

/// A sticker on a message. The image downloads after the row shows. Until
/// then, and when the download fails, the row shows "[sticker]".
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Sticker {
    /// Pixel size from the message. Zero when the message has none.
    pub width: u32,
    pub height: u32,
    pub media: MediaRef,
    /// A photo message, not a sticker. Its download is made smaller to fit
    /// the pipe, and it shows at its own size, with its caption.
    pub photo: bool,
}

impl Sticker {
    fn photo(&self, bytes: &ImageBytes) -> MessagePhoto {
        // A photo shows at the size of its image. The bytes are a JPEG, PNG,
        // or WebP that fits the pipe, so only the header is read.
        if self.photo
            && let Some(photo) = thinwire_protocol::photo_resize::photo_from_bytes(bytes.as_slice())
        {
            return photo;
        }
        let (width, height) = sticker_side(self.width, self.height);
        MessagePhoto {
            width,
            height,
            mini: None,
            sizes: vec![PhotoSize {
                width,
                height,
                file_id: None,
                path: None,
                bytes: Some(bytes.clone()),
            }],
            has_spoiler: false,
            is_secret: false,
            failed: false,
        }
    }
}

/// A voice note or an audio file on a message. It downloads when the user
/// presses Play.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Voice {
    /// Zero when the message does not say.
    pub seconds: u32,
    pub media: MediaRef,
}

/// The handle of the audio of one message for `DownloadVoice`. The same
/// message has the same handle in every run, so a row from before a helper
/// restart still downloads. Always positive.
pub(super) fn voice_file_id(jid: &str, id: &str) -> i32 {
    let low = u32::try_from(hash_of(&(jid, id)) & 0x7fff_ffff).unwrap_or(1);
    i32::try_from(low).unwrap_or(1).max(1)
}

/// The display size of a sticker: the longest side is [`STICKER_SIDE`]. A
/// sticker with no size is a square.
fn sticker_side(width: u32, height: u32) -> (u32, u32) {
    if width == 0 || height == 0 {
        return (STICKER_SIDE, STICKER_SIDE);
    }
    let long = width.max(height);
    let scale = |side: u32| {
        u32::try_from(u64::from(side) * u64::from(STICKER_SIDE) / u64::from(long))
            .unwrap_or(STICKER_SIDE)
            .max(1)
    };
    (scale(width), scale(height))
}

/// One media download the session must start. `Debug` shows no JID.
#[derive(Clone, PartialEq, Eq)]
pub(super) struct MediaRequest {
    pub chat_jid: String,
    pub id: String,
    pub media: MediaRef,
}

impl fmt::Debug for MediaRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MediaRequest(<redacted>)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum MediaState {
    Requested,
    Ready(ImageBytes),
    Failed,
}

/// One text-like message from history sync or live traffic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WaMessage {
    pub chat_jid: String,
    pub id: String,
    pub from_me: bool,
    /// Sender push name, when the server sent one.
    pub sender_name: Option<String>,
    /// Sender JID. In a DM this is the chat JID. Used when no name is known.
    pub sender_jid: Option<String>,
    /// Plain text. Media becomes a short label.
    pub body: String,
    /// Unix seconds.
    pub timestamp: i64,
    /// The sticker of a sticker message. `body` is then "[sticker]".
    pub sticker: Option<Sticker>,
    /// The audio of a voice note or an audio message.
    pub voice: Option<Voice>,
    /// The id of the message that this one replies to.
    pub reply_to: Option<String>,
    /// The reactions: who reacted ([`ME`] or a JID) and the emoji. Sorted by
    /// who, one entry for each person.
    pub reactions: Vec<(String, String)>,
}

/// Who reacted, for a reaction of this account.
pub(super) const ME: &str = "me";

/// One reaction from a live message. An empty `emoji` removes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WaReaction {
    pub chat_jid: String,
    pub target_id: String,
    /// [`ME`] for this account, else the sender JID.
    pub reactor: String,
    pub emoji: String,
}

/// The key and the text of one cached message, for a reply, a reaction, or
/// a delete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TargetRef {
    pub id: String,
    pub from_me: bool,
    pub sender_jid: Option<String>,
    pub body: String,
    pub timestamp: i64,
}

/// Set the reaction of `reactor`. An empty `emoji` removes it.
pub(super) fn set_reaction(reactions: &mut Vec<(String, String)>, reactor: &str, emoji: &str) {
    reactions.retain(|(who, _)| who != reactor);
    if !emoji.is_empty() {
        reactions.push((reactor.to_owned(), emoji.to_owned()));
        reactions.sort();
    }
}

/// The counts of each emoji, in the order of the first person who chose it.
pub(super) fn reaction_counts(reactions: &[(String, String)]) -> Vec<thinwire_protocol::Reaction> {
    let mut counts: Vec<thinwire_protocol::Reaction> = Vec::new();
    for (who, emoji) in reactions {
        match counts.iter_mut().find(|reaction| &reaction.emoji == emoji) {
            Some(reaction) => {
                reaction.count = reaction.count.saturating_add(1);
                reaction.mine |= who == ME;
            }
            None => counts.push(thinwire_protocol::Reaction {
                emoji: emoji.clone(),
                count: 1,
                mine: who == ME,
            }),
        }
    }
    counts
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HistoryAnchor {
    pub id: String,
    pub from_me: bool,
    pub timestamp: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct OlderDecision {
    pub events: Vec<AdapterEvent>,
    pub more_cached: bool,
    pub anchor: Option<HistoryAnchor>,
    pub ended: bool,
}

impl OlderDecision {
    pub(super) fn closed() -> Self {
        Self {
            events: Vec::new(),
            more_cached: false,
            anchor: None,
            ended: true,
        }
    }
}

/// One conversation from a history-sync chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HistoryChat {
    pub jid: String,
    pub name: Option<String>,
    pub unread: u32,
    /// Unix seconds of the last activity.
    pub timestamp: i64,
    pub messages: Vec<WaMessage>,
    /// The phone's mute setting for this chat, from the history sync.
    pub mute: Mute,
}

/// A WhatsApp chat mute, as the phone set it (#32). A muted chat does not
/// notify.
#[cfg_attr(not(any(test, feature = "whatsapp-web")), allow(dead_code))]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
pub(super) enum Mute {
    #[default]
    Off,
    Forever,
    /// Muted until this Unix time in milliseconds.
    UntilMs(i64),
}

/// A positive mute end below this is in seconds, not milliseconds. The
/// history sync field names no unit; 10^11 s is the year 5138.
#[cfg_attr(not(any(test, feature = "whatsapp-web")), allow(dead_code))]
const SECONDS_BELOW: i64 = 100_000_000_000;

#[cfg_attr(not(any(test, feature = "whatsapp-web")), allow(dead_code))]
fn end_ms(end: i64) -> i64 {
    if end < SECONDS_BELOW {
        end.saturating_mul(1000)
    } else {
        end
    }
}

impl Mute {
    /// From an app-state `MuteAction`: `mute_end_timestamp` is -1 for no
    /// end, 0 for unmuted, and a positive end time.
    #[cfg_attr(not(any(test, feature = "whatsapp-web")), allow(dead_code))]
    pub(super) fn from_action(muted: Option<bool>, end: Option<i64>) -> Self {
        match (muted, end) {
            (Some(false), _) | (None | Some(true), Some(0)) | (None, None) => Self::Off,
            (Some(true), None) => Self::Forever,
            (_, Some(end)) if end < 0 => Self::Forever,
            (_, Some(end)) => Self::UntilMs(end_ms(end)),
        }
    }

    /// From the history sync `Conversation.mute_end_time`: none or 0 is not
    /// muted; a value past `i64::MAX` has no end.
    #[cfg_attr(not(any(test, feature = "whatsapp-web")), allow(dead_code))]
    pub(super) fn from_history(end: Option<u64>) -> Self {
        match end {
            None | Some(0) => Self::Off,
            Some(end) => i64::try_from(end).map_or(Self::Forever, |end| Self::UntilMs(end_ms(end))),
        }
    }

    pub(super) const fn is_on(self, now_ms: i64) -> bool {
        match self {
            Self::Off => false,
            Self::Forever => true,
            Self::UntilMs(end) => end > now_ms,
        }
    }
}

pub(super) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

#[derive(Debug, Clone, Default)]
struct ChatRecord {
    name: Option<String>,
    unread: u32,
    timestamp: i64,
    /// Sorted by `(timestamp, id)`. Capped at [`MESSAGES_PER_CHAT`].
    messages: Vec<WaMessage>,
    /// Local `pending:` ids whose send failed. The user can resend them.
    failed: HashSet<String>,
    /// Local ids of photo and video sends. Their files are not kept, so a
    /// failed one cannot be sent again.
    media_sends: HashSet<String>,
    /// The user saw this chat up to this message time (Unix seconds). A later
    /// history chunk carries the phone's older unread count, so a chat read
    /// here changes its count only by new messages (#98). A message older
    /// than this time does not count. It only moves forward (#158 review).
    read_at: Option<i64>,
    /// Inbound ids handled since the read point, with their time and arrival
    /// number: counted, or seen in the read second. A repeat does not count
    /// again, also after its row left the capped cache (Codex r4131089163,
    /// r4131622220). An id older than the read point goes: the read point
    /// refuses it already (Codex r4132341446, #158 review). At most
    /// [`HANDLED_PER_CHAT`] ids.
    handled: HashMap<String, (i64, u64)>,
    /// Arrival number of the newest id in `handled`.
    handled_seq: u64,
    /// Server ids that still carry the local send clock. `confirm_send`
    /// keeps that clock until a server or live copy arrives. The read point
    /// skips them (Codex r4132341439).
    optimistic: HashSet<String>,
    /// Newest activity time from history sync. A dropped send falls back to
    /// it (Codex r4131089177).
    history_at: i64,
    history_ended: bool,
    /// Oldest key of the capped window. Rows older than it were fetched
    /// on demand and stay for this process. Not saved.
    page_floor: Option<(i64, String)>,
    mute: Mute,
    /// A live `MuteUpdate` of this connection set `mute`: an older history
    /// chunk does not change it again. A reconnect clears it (#168 item 12).
    mute_live: bool,
}

impl ChatRecord {
    /// The user sees every message of this chat now.
    fn mark_read(&mut self) {
        self.unread = 0;
        let read_at = self.count_from(self.server_time());
        // Rows of the read second were seen; a later copy must not count. A
        // new id in that second still counts. An older id needs no entry:
        // the read point refuses it (Codex r4132341446, #158 review).
        let seen: Vec<(String, i64)> = self
            .messages
            .iter()
            .filter(|row| !row.from_me && row.timestamp >= read_at)
            .map(|row| (row.id.clone(), row.timestamp))
            .collect();
        for (id, timestamp) in seen {
            self.remember(&id, timestamp);
        }
    }

    /// Move the read point forward to `point` and drop the handled ids older
    /// than it. Returns the read point. It never moves back.
    fn count_from(&mut self, point: i64) -> i64 {
        let point = self.read_at.map_or(point, |old| old.max(point));
        self.read_at = Some(point);
        self.handled.retain(|_, (timestamp, _)| *timestamp >= point);
        point
    }

    /// Note one inbound id as handled. `false` if it was handled before.
    fn remember(&mut self, id: &str, timestamp: i64) -> bool {
        if self.handled.contains_key(id) {
            return false;
        }
        self.handled_seq += 1;
        self.handled
            .insert(id.to_string(), (timestamp, self.handled_seq));
        if self.handled.len() > HANDLED_PER_CHAT {
            self.forget_older_half();
        }
        true
    }

    /// Keep the newer half of `handled`, by time and then by arrival, so a
    /// chat with equal times keeps a bound too (#158 review). The read point
    /// stays: a late new message older than the ids kept still counts. A
    /// repeat of a dropped id can count again. That needs more than
    /// [`HANDLED_PER_CHAT`] ids since the read (#168 item 5).
    fn forget_older_half(&mut self) {
        let mut order: Vec<(i64, u64)> = self.handled.values().copied().collect();
        order.sort_unstable();
        let keep = order[order.len() / 2];
        self.handled.retain(|_, entry| *entry >= keep);
    }

    /// The newest server time of this chat: the newest row that is not a
    /// local `pending:` send and not a confirmed send still on the local
    /// clock. A local send time can be ahead of the server's clock, and a
    /// later inbound message with an earlier server time must still count
    /// (#158 review, Codex r4132341439). The history time counts only with
    /// no such row: it can be newer than the rows of a chunk, and the user
    /// did not see a message between them (#168 item 6).
    fn server_time(&self) -> i64 {
        self.messages
            .iter()
            .filter(|row| !row.id.starts_with(PENDING_PREFIX) && !self.optimistic.contains(&row.id))
            .map(|row| row.timestamp)
            .max()
            .unwrap_or(self.history_at)
    }

    /// Count one new inbound message of a chat read here, from history or
    /// live. `true` if it adds to unread: not stored, not handled since the
    /// read, and not older than the read point. Both paths use this one rule
    /// (#158 review), so `handled` keeps no id older than the read point.
    fn counts_after_read(&mut self, message: &WaMessage) -> bool {
        let Some(read_at) = self.read_at else {
            return false;
        };
        if message.from_me || message.timestamp < read_at {
            return false;
        }
        let stored = self.messages.iter().any(|row| row.id == message.id);
        !stored && self.remember(&message.id, message.timestamp)
    }

    /// The chat time from what is still here: the newest row, or the
    /// history time.
    fn restore_time(&mut self) {
        let newest = self.messages.iter().map(|row| row.timestamp).max();
        self.timestamp = newest.unwrap_or(0).max(self.history_at);
    }
}

// region: disk cache

/// One saved message. A pending or failed send is not saved.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub(super) struct SavedMessage {
    pub id: String,
    pub from_me: bool,
    pub sender_name: Option<String>,
    pub sender_jid: Option<String>,
    pub body: String,
    pub timestamp: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reactions: Vec<(String, String)>,
}

/// One saved chat: the row and its newest messages.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub(super) struct SavedChat {
    pub name: Option<String>,
    pub unread: u32,
    pub timestamp: i64,
    pub read_at: Option<i64>,
    pub history_at: i64,
    pub mute: Mute,
    pub messages: Vec<SavedMessage>,
}

/// The chat list that a saved session loads at its start.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct SavedInbox {
    pub chats: Vec<(String, SavedChat)>,
    pub names: Vec<(String, String)>,
}

/// What changed since the last save: chats to write, chats to remove, and
/// the push names when they changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct InboxChanges {
    pub chats: Vec<(String, u64, SavedChat)>,
    pub removed: Vec<String>,
    pub names: Option<(u64, Vec<(String, String)>)>,
}

impl InboxChanges {
    pub(super) fn is_empty(&self) -> bool {
        self.chats.is_empty() && self.removed.is_empty() && self.names.is_none()
    }
}

/// Hashes of the chats and the names that the disk holds now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct SavedHashes {
    chats: HashMap<String, u64>,
    names: u64,
}

impl SavedHashes {
    /// The disk took these changes.
    pub(super) fn apply(&mut self, changes: &InboxChanges) {
        for (jid, hash, _) in &changes.chats {
            self.chats.insert(jid.clone(), *hash);
        }
        for jid in &changes.removed {
            self.chats.remove(jid);
        }
        if let Some((hash, _)) = &changes.names {
            self.names = *hash;
        }
    }
}

/// Writes the chat cache. The live client keeps it in an encrypted file
/// beside the device store. Tests use a fake.
pub(super) trait InboxStore: Send + Sync {
    /// Write the changes in one transaction.
    fn save(&self, changes: &InboxChanges) -> Result<(), ()>;
}

fn saved_message(message: &WaMessage) -> SavedMessage {
    SavedMessage {
        id: message.id.clone(),
        from_me: message.from_me,
        sender_name: message.sender_name.clone(),
        sender_jid: message.sender_jid.clone(),
        body: message.body.clone(),
        timestamp: message.timestamp,
        reply_to: message.reply_to.clone(),
        reactions: message.reactions.clone(),
    }
}

fn hash_of(value: &impl std::hash::Hash) -> u64 {
    use std::hash::{BuildHasher, BuildHasherDefault, DefaultHasher};
    BuildHasherDefault::<DefaultHasher>::default().hash_one(value)
}

impl ChatRecord {
    fn saved(&self) -> SavedChat {
        SavedChat {
            name: self.name.clone(),
            unread: self.unread,
            timestamp: self.timestamp,
            read_at: self.read_at,
            history_at: self.history_at,
            mute: self.mute,
            messages: self
                .messages
                .iter()
                .filter(|row| !row.id.starts_with(PENDING_PREFIX))
                .map(saved_message)
                .collect(),
        }
    }
}

// endregion: disk cache

/// In-memory WhatsApp inbox for one linked device.
#[derive(Debug, Default)]
pub(super) struct Inbox {
    chats: HashMap<String, ChatRecord>,
    /// Push names from history sync, keyed by JID.
    names: HashMap<String, String>,
    open: Option<String>,
    next_pending: u64,
    /// Chats sent by the chat-list pages so far (newest first).
    listed: usize,
    /// Sticker images by chat JID and message id.
    media: HashMap<(String, String), MediaState>,
    /// Ready images, oldest first, for the [`MEDIA_CACHE`] limit.
    media_ready: VecDeque<(String, String)>,
}

impl Inbox {
    /// The chats and the names that changed since `saved`. A chat with no
    /// row any more is in `removed`.
    pub(super) fn changes(&self, saved: &SavedHashes) -> InboxChanges {
        let mut changes = InboxChanges::default();
        for (jid, record) in &self.chats {
            let chat = record.saved();
            let hash = hash_of(&chat);
            if saved.chats.get(jid) != Some(&hash) {
                changes.chats.push((jid.clone(), hash, chat));
            }
        }
        changes.removed = saved
            .chats
            .keys()
            .filter(|jid| !self.chats.contains_key(*jid))
            .cloned()
            .collect();
        let mut names: Vec<(String, String)> = self
            .names
            .iter()
            .map(|(jid, name)| (jid.clone(), name.clone()))
            .collect();
        names.sort();
        let hash = hash_of(&names);
        if hash != saved.names {
            changes.names = Some((hash, names));
        }
        changes
    }

    /// Load the saved chat list of this device. A chat the inbox already
    /// has stays as it is. Returns the hashes of what the disk holds.
    pub(super) fn restore(&mut self, saved: SavedInbox) -> SavedHashes {
        let mut hashes = SavedHashes::default();
        for (jid, name) in saved.names {
            self.names.entry(jid).or_insert(name);
        }
        let mut names: Vec<(String, String)> = self
            .names
            .iter()
            .map(|(jid, name)| (jid.clone(), name.clone()))
            .collect();
        names.sort();
        hashes.names = hash_of(&names);
        for (jid, chat) in saved.chats {
            if !is_inbox_chat(&jid) || self.chats.contains_key(&jid) {
                continue;
            }
            hashes.chats.insert(jid.clone(), hash_of(&chat));
            let record = ChatRecord {
                name: chat.name,
                unread: chat.unread,
                timestamp: chat.timestamp,
                read_at: chat.read_at,
                history_at: chat.history_at,
                mute: chat.mute,
                messages: chat
                    .messages
                    .into_iter()
                    .map(|message| WaMessage {
                        chat_jid: jid.clone(),
                        id: message.id,
                        from_me: message.from_me,
                        sender_name: message.sender_name,
                        sender_jid: message.sender_jid,
                        body: message.body,
                        timestamp: message.timestamp,
                        sticker: None,
                        voice: None,
                        reply_to: message.reply_to,
                        reactions: message.reactions,
                    })
                    .collect(),
                ..ChatRecord::default()
            };
            self.chats.insert(jid, record);
        }
        hashes
    }

    pub(super) fn clear(&mut self) -> Vec<AdapterEvent> {
        let removed = self
            .chats
            .keys()
            .map(|jid| AdapterEvent::ConversationRemoved {
                protocol: ProtocolId::WhatsApp,
                id: conversation_id(jid),
            })
            .collect();
        *self = Self::default();
        removed
    }

    /// The newest chats of the inbox, newest first, at most `limit`.
    pub(super) fn recent_chats(&self, limit: usize) -> Vec<String> {
        let mut chats: Vec<(&String, i64)> = self
            .chats
            .iter()
            .filter(|(jid, _)| is_inbox_chat(jid))
            .map(|(jid, record)| (jid, record.timestamp))
            .collect();
        chats.sort_by_key(|(_, timestamp)| std::cmp::Reverse(*timestamp));
        chats
            .into_iter()
            .take(limit)
            .map(|(jid, _)| jid.clone())
            .collect()
    }

    /// The subject of a group from the server. A known chat gets it and
    /// comes again. A group with no chat here is not added.
    pub(super) fn set_group_name(&mut self, jid: &str, name: &str) -> Option<AdapterEvent> {
        let name = name.trim();
        if !is_group(jid) || name.is_empty() {
            return None;
        }
        let record = self.chats.get_mut(jid)?;
        if record.name.as_deref() == Some(name) {
            return None;
        }
        record.name = Some(name.to_string());
        self.upsert_event(jid)
    }

    pub(super) fn unnamed_groups(&self) -> Vec<String> {
        let mut jids: Vec<String> = self
            .chats
            .iter()
            .filter(|(jid, record)| {
                is_group(jid)
                    && record
                        .name
                        .as_deref()
                        .is_none_or(|name| name.trim().is_empty())
            })
            .map(|(jid, _)| jid.clone())
            .collect();
        jids.sort();
        jids
    }

    /// The audio of the message with the handle `file_id`, while the
    /// message is in the cache.
    pub(super) fn voice_media(&self, file_id: i32) -> Option<MediaRef> {
        self.chats.iter().find_map(|(jid, record)| {
            record
                .messages
                .iter()
                .find(|message| {
                    message.voice.is_some() && voice_file_id(jid, &message.id) == file_id
                })
                .and_then(|message| message.voice.as_ref())
                .map(|voice| voice.media.clone())
        })
    }

    /// The sticker downloads for the rows in `events`. Each sticker asks
    /// one time: a later row of the same message asks again only after its
    /// image left the cache.
    ///
    /// A photo of a history row asks only when `history_photos` is set: the
    /// chat is open, or the user loads an older page. A history sync at a
    /// pairing does not download every photo.
    pub(super) fn media_requests(
        &mut self,
        events: &[AdapterEvent],
        history_photos: bool,
    ) -> Vec<MediaRequest> {
        let mut out = Vec::new();
        for event in events {
            let message = match event {
                AdapterEvent::MessageReceived { message }
                | AdapterEvent::MessageReplaced { message, .. } => message,
                _ => continue,
            };
            let Some(jid) = parse_conversation_id(&message.conversation_id) else {
                continue;
            };
            let Some(sticker) = self
                .chats
                .get(jid)
                .and_then(|record| record.messages.iter().find(|row| row.id == message.id))
                .and_then(|row| row.sticker.as_ref())
            else {
                continue;
            };
            if sticker.photo && message.arrival == Arrival::History && !history_photos {
                continue;
            }
            let key = (jid.to_string(), message.id.clone());
            if self.media.contains_key(&key) {
                continue;
            }
            out.push(MediaRequest {
                chat_jid: key.0.clone(),
                id: key.1.clone(),
                media: sticker.media.clone(),
            });
            self.media.insert(key, MediaState::Requested);
        }
        out
    }

    /// A sticker download ended. With an image, the row comes again with
    /// it, when the message is still in the cache. A failed download keeps
    /// the "[sticker]" row and does not ask again.
    pub(super) fn finish_media(
        &mut self,
        jid: &str,
        id: &str,
        bytes: Option<Vec<u8>>,
    ) -> Option<AdapterEvent> {
        let key = (jid.to_string(), id.to_string());
        if self.media.get(&key) != Some(&MediaState::Requested) {
            // A reset dropped the request. The row is gone or new.
            return None;
        }
        let Some(bytes) = bytes.filter(|bytes| !bytes.is_empty() && bytes.len() <= MEDIA_MAX_BYTES)
        else {
            self.media.insert(key, MediaState::Failed);
            return None;
        };
        self.media
            .insert(key.clone(), MediaState::Ready(ImageBytes::new(bytes)));
        self.media_ready.push_back(key);
        while self.media_ready.len() > MEDIA_CACHE {
            if let Some(old) = self.media_ready.pop_front() {
                self.media.remove(&old);
            }
        }
        let message = self
            .chats
            .get(jid)?
            .messages
            .iter()
            .find(|row| row.id == id)?;
        Some(AdapterEvent::MessageReceived {
            message: self.chat_message(message, Arrival::History),
        })
    }

    /// Merge one history-sync chunk. Returns upserts for the chats it touched.
    pub(super) fn apply_history(
        &mut self,
        chats: Vec<HistoryChat>,
        push_names: Vec<(String, String)>,
    ) -> Vec<AdapterEvent> {
        for (jid, name) in push_names {
            let name = name.trim().to_string();
            if !name.is_empty() {
                self.names.insert(jid, name);
            }
        }
        let mut touched = Vec::new();
        for chat in chats {
            if !is_inbox_chat(&chat.jid) {
                continue;
            }
            let record = self.chats.entry(chat.jid.clone()).or_default();
            if let Some(name) = chat.name.map(|name| name.trim().to_string())
                && !name.is_empty()
            {
                record.name = Some(name);
            }
            record.history_at = record.history_at.max(chat.timestamp);
            record.timestamp = record.timestamp.max(chat.timestamp);
            if !record.mute_live {
                record.mute = chat.mute;
            }
            let mut added: u32 = 0;
            for message in chat.messages {
                remember_name(&mut self.names, &message);
                record.timestamp = record.timestamp.max(message.timestamp);
                // A history copy carries the server clock (Codex r4132341439).
                record.optimistic.remove(&message.id);
                if record.counts_after_read(&message) {
                    added = added.saturating_add(1);
                }
                insert_message(&mut record.messages, &mut record.page_floor, message);
            }
            // The user reads the open chat, so a later chunk keeps it read.
            // A chat read here changes its count only by new messages (#98):
            // the chunk's count is the phone's, from before the read. Else
            // keep live increments: a chunk can carry an older count.
            if self.open.as_deref() == Some(chat.jid.as_str()) {
                record.mark_read();
            } else if record.read_at.is_some() {
                record.unread = record.unread.saturating_add(added);
            } else {
                record.unread = record.unread.max(chat.unread);
            }
            touched.push(chat.jid);
        }
        touched.sort();
        touched.dedup();
        touched
            .iter()
            .filter_map(|jid| self.upsert_event(jid))
            .collect()
    }

    /// Merge live messages. Every message goes to the UI; unread grows only
    /// for inbound messages outside the open chat.
    /// Live reactions. Each one changes the counts of a cached message. A
    /// reaction to a message that is not in the cache is dropped.
    pub(super) fn apply_reactions(&mut self, reactions: Vec<WaReaction>) -> Vec<AdapterEvent> {
        let mut events = Vec::new();
        for reaction in reactions {
            let Some(message) = self.chats.get_mut(&reaction.chat_jid).and_then(|record| {
                record
                    .messages
                    .iter_mut()
                    .find(|row| row.id == reaction.target_id)
            }) else {
                continue;
            };
            set_reaction(&mut message.reactions, &reaction.reactor, &reaction.emoji);
            events.push(AdapterEvent::MessageReactions {
                protocol: ProtocolId::WhatsApp,
                conversation_id: conversation_id(&reaction.chat_jid),
                message_id: reaction.target_id.clone(),
                reactions: reaction_counts(&message.reactions),
            });
        }
        events
    }

    /// The key and text of a cached message that the server has. A pending
    /// send has no server id yet.
    pub(super) fn target(&self, jid: &str, id: &str) -> Option<TargetRef> {
        if id.starts_with(PENDING_PREFIX) {
            return None;
        }
        let message = self
            .chats
            .get(jid)?
            .messages
            .iter()
            .find(|row| row.id == id)?;
        Some(TargetRef {
            id: message.id.clone(),
            from_me: message.from_me,
            sender_jid: message.sender_jid.clone(),
            body: message.body.clone(),
            timestamp: message.timestamp,
        })
    }

    /// Mark the cached message `id` as a reply to `reply_to`. Returns the new
    /// row.
    pub(super) fn set_reply(
        &mut self,
        jid: &str,
        id: &str,
        reply_to: &str,
    ) -> Option<AdapterEvent> {
        let message = self
            .chats
            .get_mut(jid)?
            .messages
            .iter_mut()
            .find(|row| row.id == id)?;
        message.reply_to = Some(reply_to.to_owned());
        let message = message.clone();
        Some(AdapterEvent::MessageReceived {
            message: self.chat_message(&message, Arrival::History),
        })
    }

    /// Drop one message from the cache. The row leaves the thread.
    pub(super) fn remove_message(&mut self, jid: &str, id: &str) -> AdapterEvent {
        if let Some(record) = self.chats.get_mut(jid) {
            record.messages.retain(|row| row.id != id);
        }
        AdapterEvent::MessagesRemoved {
            protocol: ProtocolId::WhatsApp,
            conversation_id: conversation_id(jid),
            message_ids: vec![id.to_owned()],
        }
    }

    pub(super) fn apply_messages(&mut self, messages: Vec<WaMessage>) -> Vec<AdapterEvent> {
        let mut events = Vec::new();
        for message in messages {
            if !is_inbox_chat(&message.chat_jid) || message.id.is_empty() {
                continue;
            }
            remember_name(&mut self.names, &message);
            let jid = message.chat_jid.clone();
            let is_open = self.open.as_deref() == Some(jid.as_str());
            let record = self.chats.entry(jid.clone()).or_default();
            // A live copy carries the server clock (Codex r4132341439).
            record.optimistic.remove(&message.id);
            let known = record.messages.iter().any(|row| row.id == message.id);
            // A mute that ended by its time: the row goes first, so this
            // message sees the chat as not muted.
            let mute_ended = !known && matches!(record.mute, Mute::UntilMs(end) if end <= now_ms());
            if mute_ended {
                // Once: a later message sends no extra row (#168 item 13).
                record.mute = Mute::Off;
            }
            // A repeat of a stored id is a refresh. Only the first sight of a
            // pushed message is live, so a history replay cannot notify (#32).
            let arrival = if known {
                Arrival::History
            } else {
                Arrival::Live
            };
            // After a local read, an id counts once: a redelivery after its
            // row left the capped cache is not new (Codex r4131622220). Only
            // an id that can count goes into `handled`, and the history rule
            // applies (#158 review).
            let counts = !known
                && !message.from_me
                && !is_open
                && (record.read_at.is_none() || record.counts_after_read(&message));
            if counts {
                record.unread = record.unread.saturating_add(1);
            }
            record.timestamp = record.timestamp.max(message.timestamp);
            insert_message(
                &mut record.messages,
                &mut record.page_floor,
                message.clone(),
            );
            if is_open {
                record.mark_read();
            }
            if mute_ended && let Some(upsert) = self.upsert_event(&jid) {
                events.push(upsert);
            }
            events.push(AdapterEvent::MessageReceived {
                message: self.chat_message(&message, arrival),
            });
            if let Some(upsert) = self.upsert_event(&jid) {
                events.push(upsert);
            }
        }
        events
    }

    /// Newest chats first, at most [`CHAT_PAGE`].
    /// The first page of the chat list: newest first, at most [`CHAT_PAGE`].
    /// Starts the page cursor again.
    pub(super) fn chat_page(&mut self) -> Vec<AdapterEvent> {
        self.listed = 0;
        self.next_chat_page()
    }

    /// The next page after the chats already sent (Codex r4103855629). Empty
    /// after the last page. A chat that moved between pages can come twice;
    /// an upsert is idempotent. New activity sends its own upsert anyway.
    pub(super) fn next_chat_page(&mut self) -> Vec<AdapterEvent> {
        let mut jids: Vec<&String> = self.chats.keys().collect();
        jids.sort_by(|a, b| {
            let left = self.chats.get(*a).map_or(0, |row| row.timestamp);
            let right = self.chats.get(*b).map_or(0, |row| row.timestamp);
            right.cmp(&left).then_with(|| a.cmp(b))
        });
        let page: Vec<AdapterEvent> = jids
            .into_iter()
            .skip(self.listed)
            .take(CHAT_PAGE)
            .filter_map(|jid| self.upsert_event(jid))
            .collect();
        self.listed += page.len();
        page
    }

    /// Mark a chat open and read. Returns the upsert and the latest messages.
    pub(super) fn open_chat(&mut self, jid: &str) -> Option<Vec<AdapterEvent>> {
        let record = self.chats.get_mut(jid)?;
        record.mark_read();
        self.open = Some(jid.to_string());
        let mut events = Vec::new();
        if let Some(upsert) = self.upsert_event(jid) {
            events.push(upsert);
        }
        let record = self.chats.get(jid)?;
        let skip = record.messages.len().saturating_sub(OPEN_CHAT_MESSAGES);
        events.extend(record.messages.iter().skip(skip).map(|message| {
            AdapterEvent::MessageReceived {
                message: self.chat_message(message, Arrival::History),
            }
        }));
        Some(events)
    }

    pub(super) fn older_decision(&self, jid: &str, before: &str, limit: usize) -> OlderDecision {
        let Some(record) = self.chats.get(jid) else {
            return OlderDecision::closed();
        };
        let Some(end) = record.messages.iter().position(|row| row.id == before) else {
            return OlderDecision::closed();
        };
        let anchor = &record.messages[end];
        let start = end.saturating_sub(limit);
        let events = record.messages[start..end]
            .iter()
            .map(|message| AdapterEvent::MessageReceived {
                message: self.chat_message(message, Arrival::History),
            })
            .collect();
        OlderDecision {
            events,
            more_cached: start > 0,
            anchor: Some(HistoryAnchor {
                id: anchor.id.clone(),
                from_me: anchor.from_me,
                timestamp: anchor.timestamp,
            }),
            ended: record.history_ended,
        }
    }

    pub(super) fn history_row(&self, message: &WaMessage) -> AdapterEvent {
        AdapterEvent::MessageReceived {
            message: self.chat_message(message, Arrival::History),
        }
    }

    pub(super) fn mark_history_ended(&mut self, jid: &str) {
        if let Some(record) = self.chats.get_mut(jid) {
            record.history_ended = true;
        }
    }

    /// The chat the user looks at now (`None`: no WhatsApp chat). Messages
    /// in the viewed chat do not raise unread; a chat the user left counts
    /// again. Viewing a known chat marks it read and returns its upsert.
    pub(super) fn view(&mut self, jid: Option<&str>) -> Option<AdapterEvent> {
        // The chat the user leaves was read up to its newest message. Same
        // rule as an open chat, so its seen ids stay handled (#158 review).
        if let Some(left) = self.open.take()
            && Some(left.as_str()) != jid
            && let Some(record) = self.chats.get_mut(&left)
        {
            record.mark_read();
        }
        self.open = jid.map(str::to_string);
        let jid = jid?;
        let record = self.chats.get_mut(jid)?;
        let had_unread = record.unread > 0;
        record.mark_read();
        if had_unread {
            self.upsert_event(jid)
        } else {
            None
        }
    }

    #[must_use]
    pub(super) fn knows_chat(&self, jid: &str) -> bool {
        self.chats.contains_key(jid)
    }

    /// Show an outgoing message at once under a local id. The send result
    /// replaces it with [`Self::confirm_send`] or marks it failed with
    /// [`Self::fail_send`].
    /// Returns the local id, the pending row, and the chat upsert. The shell
    /// takes the sidebar preview and order from the upsert.
    pub(super) fn begin_send(
        &mut self,
        jid: &str,
        body: &str,
        now: i64,
    ) -> (String, AdapterEvent, Option<AdapterEvent>) {
        self.next_pending += 1;
        let pending = format!("{PENDING_PREFIX}{}", self.next_pending);
        let message = WaMessage {
            chat_jid: jid.to_string(),
            id: pending.clone(),
            from_me: true,
            sender_name: None,
            sender_jid: None,
            body: body.to_string(),
            timestamp: now,
            sticker: None,
            voice: None,
            reactions: Vec::new(),
            reply_to: None,
        };
        let record = self.chats.entry(jid.to_string()).or_default();
        record.timestamp = record.timestamp.max(now);
        insert_message(
            &mut record.messages,
            &mut record.page_floor,
            message.clone(),
        );
        (
            pending,
            AdapterEvent::MessageReceived {
                message: self.chat_message(&message, Arrival::History),
            },
            self.upsert_event(jid),
        )
    }

    /// The pending row `pending` is a send of photos or videos.
    pub(super) fn mark_media_send(&mut self, jid: &str, pending: &str) {
        if let Some(record) = self.chats.get_mut(jid) {
            record.media_sends.insert(pending.to_string());
        }
    }

    pub(super) fn confirm_send(
        &mut self,
        jid: &str,
        pending: &str,
        server_id: String,
    ) -> Vec<AdapterEvent> {
        let Some(record) = self.chats.get_mut(jid) else {
            return Vec::new();
        };
        // An echo may already be stored under the server id, with its clock.
        // Keep that row. Renaming the pending row onto the same id and keeping
        // the first copy drops the echo when the local clock is behind, and
        // the read point then treats the local clock as server time (Codex
        // r4132803796).
        let server_copy = record
            .messages
            .iter()
            .find(|row| row.id == server_id && row.id != pending)
            .cloned();
        let Some(pending_at) = record.messages.iter().position(|row| row.id == pending) else {
            return Vec::new();
        };
        record.failed.remove(pending);
        // An id whose row left the cache has no clock to skip (#158 review).
        record
            .optimistic
            .retain(|id| record.messages.iter().any(|row| row.id == *id));
        let message = if let Some(server) = server_copy {
            record.messages.remove(pending_at);
            // The pending row moved the chat time to the local clock; the
            // echo keeps the server clock (#168 item 7).
            record.restore_time();
            server
        } else {
            let row = &mut record.messages[pending_at];
            row.id.clone_from(&server_id);
            let message = row.clone();
            // The row keeps the local clock until a server copy arrives.
            record.optimistic.insert(server_id);
            message
        };
        let mut events = vec![AdapterEvent::MessageReplaced {
            protocol: ProtocolId::WhatsApp,
            conversation_id: conversation_id(jid),
            old_id: pending.to_string(),
            message: self.chat_message(&message, Arrival::History),
        }];
        events.extend(self.upsert_event(jid));
        events
    }

    /// Remove the row of a new send that the server did not accept. The
    /// text stays in the compose field (`SendRejected`), so no failed row
    /// stays next to it (#98).
    pub(super) fn drop_send(&mut self, jid: &str, pending: &str) -> Vec<AdapterEvent> {
        let Some(record) = self.chats.get_mut(jid) else {
            return Vec::new();
        };
        let before = record.messages.len();
        record.messages.retain(|row| row.id != pending);
        record.failed.remove(pending);
        if record.messages.len() == before {
            return Vec::new();
        }
        // The send moved the chat time; the dropped row takes it back.
        record.restore_time();
        let mut events = vec![AdapterEvent::MessagesRemoved {
            protocol: ProtocolId::WhatsApp,
            conversation_id: conversation_id(jid),
            message_ids: vec![pending.to_string()],
        }];
        events.extend(self.upsert_event(jid));
        events
    }

    /// Keep the row and mark it failed, so the user can resend it.
    pub(super) fn fail_send(&mut self, jid: &str, pending: &str) -> Vec<AdapterEvent> {
        let Some(record) = self.chats.get_mut(jid) else {
            return Vec::new();
        };
        if !record.messages.iter().any(|row| row.id == pending) {
            return Vec::new();
        }
        record.failed.insert(pending.to_string());
        vec![delivery_event(jid, pending, Delivery::Failed)]
    }

    /// A failed outgoing row goes back to pending. Returns its body and the
    /// delivery event, or `None` when the row is not a failed send.
    pub(super) fn retry_send(&mut self, jid: &str, id: &str) -> Option<(String, AdapterEvent)> {
        let record = self.chats.get_mut(jid)?;
        if record.media_sends.contains(id) {
            return None;
        }
        if !record.failed.remove(id) {
            return None;
        }
        let body = record
            .messages
            .iter()
            .find(|row| row.id == id)?
            .body
            .clone();
        Some((body, delivery_event(jid, id, Delivery::Pending)))
    }

    /// A new connection to the server starts: a reconnect or a new link
    /// (#168 item 12). The phone can change a mute while thinwire is
    /// offline, so a history value may be newer than a live change of the
    /// last connection. A live change of the new connection still wins over
    /// a later chunk. Call it before the first event of the connection: a
    /// reconnect can send `History` before `Connected` (Codex r4138502149).
    pub(super) fn new_connection(&mut self) {
        for record in self.chats.values_mut() {
            record.mute_live = false;
        }
    }

    /// End every timed mute whose end passed (#168 item 13). The record
    /// becomes [`Mute::Off`] and sends one row, so the shell counts its
    /// unread messages again (window title).
    pub(super) fn expire_mutes(&mut self, now_ms: i64) -> Vec<AdapterEvent> {
        let mut ended: Vec<String> = self
            .chats
            .iter_mut()
            .filter(|(_, record)| matches!(record.mute, Mute::UntilMs(end) if end <= now_ms))
            .map(|(jid, record)| {
                record.mute = Mute::Off;
                jid.clone()
            })
            .collect();
        ended.sort();
        ended
            .iter()
            .filter_map(|jid| self.upsert_event(jid))
            .collect()
    }

    /// The earliest end of a timed mute, in Unix milliseconds.
    pub(super) fn next_mute_end(&self) -> Option<i64> {
        self.chats
            .values()
            .filter_map(|record| match record.mute {
                Mute::UntilMs(end) => Some(end),
                Mute::Off | Mute::Forever => None,
            })
            .min()
    }

    /// A live mute change from the phone. Returns the new row.
    pub(super) fn apply_mute(&mut self, jid: &str, mute: Mute) -> Option<AdapterEvent> {
        if !is_inbox_chat(jid) {
            return None;
        }
        let record = self.chats.entry(jid.to_string()).or_default();
        record.mute = mute;
        record.mute_live = true;
        self.upsert_event(jid)
    }

    fn upsert_event(&self, jid: &str) -> Option<AdapterEvent> {
        let record = self.chats.get(jid)?;
        let title = self.title(jid, record);
        let preview = record
            .messages
            .last()
            .map(|message| preview(&message.body))
            .unwrap_or_default();
        Some(AdapterEvent::ConversationUpsert {
            conversation: Conversation {
                protocol: ProtocolId::WhatsApp,
                id: conversation_id(jid),
                participant: if is_group(jid) {
                    "Group".into()
                } else {
                    title.clone()
                },
                title,
                preview,
                unread: record.unread,
                order: record.timestamp,
                last_at: record.timestamp,
                is_group: is_group(jid),
                writable: true,
                placeholder: false,
                muted: record.mute.is_on(now_ms()),
                has_topics: false,
                avatar: Default::default(),
            },
        })
    }

    fn title(&self, jid: &str, record: &ChatRecord) -> String {
        if let Some(name) = &record.name {
            return name.clone();
        }
        if let Some(name) = self.names.get(jid) {
            return name.clone();
        }
        fallback_label(jid)
    }

    fn chat_message(&self, message: &WaMessage, arrival: Arrival) -> ChatMessage {
        let sender = if message.from_me {
            OUTBOUND_SENDER.to_string()
        } else if let Some(name) = message.sender_name.as_ref().filter(|name| !name.is_empty()) {
            name.clone()
        } else {
            let jid = message
                .sender_jid
                .as_deref()
                .unwrap_or(message.chat_jid.as_str());
            self.names
                .get(jid)
                .cloned()
                .unwrap_or_else(|| fallback_label(jid))
        };
        let failed = self
            .chats
            .get(&message.chat_jid)
            .is_some_and(|record| record.failed.contains(&message.id));
        let delivery = if failed {
            Delivery::Failed
        } else if message.id.starts_with(PENDING_PREFIX) {
            Delivery::Pending
        } else {
            Delivery::Sent
        };
        // A sticker with its image shows the image and no label. A photo
        // keeps its caption as the body.
        let photo = message.sticker.as_ref().and_then(|sticker| {
            match self
                .media
                .get(&(message.chat_jid.clone(), message.id.clone()))
            {
                Some(MediaState::Ready(bytes)) => Some(sticker.photo(bytes)),
                _ => None,
            }
        });
        let caption = message
            .sticker
            .as_ref()
            .is_some_and(|sticker| sticker.photo && message.body != PHOTO_LABEL);
        let body = if photo.is_some() && !caption {
            String::new()
        } else {
            message.body.clone()
        };
        ChatMessage {
            protocol: ProtocolId::WhatsApp,
            conversation_id: conversation_id(&message.chat_jid),
            id: message.id.clone(),
            sender,
            body,
            outbound: message.from_me,
            delivery,
            sent_at: message.timestamp,
            arrival,
            topic: None,
            links: Vec::new(),
            photo,
            audio: message.voice.as_ref().and_then(|voice| {
                MessageAudio::new(
                    voice.seconds,
                    voice_file_id(&message.chat_jid, &message.id),
                    None,
                )
            }),
            video: None,
            reactions: reaction_counts(&message.reactions),
            reply_to: message.reply_to.clone(),
        }
    }
}

fn delivery_event(jid: &str, id: &str, delivery: Delivery) -> AdapterEvent {
    AdapterEvent::MessageDelivery {
        protocol: ProtocolId::WhatsApp,
        conversation_id: conversation_id(jid),
        message_id: id.to_string(),
        delivery,
    }
}

#[must_use]
pub(super) fn conversation_id(jid: &str) -> String {
    format!("{CONVERSATION_PREFIX}{jid}")
}

/// Chat JID from a conversation id. Placeholder and malformed ids return `None`.
#[must_use]
pub(super) fn parse_conversation_id(id: &str) -> Option<&str> {
    let jid = id.strip_prefix(CONVERSATION_PREFIX)?;
    let (user, server) = jid.split_once('@')?;
    if user.is_empty() || server.is_empty() || jid.chars().any(char::is_whitespace) {
        return None;
    }
    Some(jid)
}

/// Status stories, broadcast lists, and channels stay out of the inbox.
fn is_inbox_chat(jid: &str) -> bool {
    !jid.is_empty()
        && jid != STATUS_BROADCAST
        && !jid.ends_with(BROADCAST_SERVER)
        && !jid.ends_with(NEWSLETTER_SERVER)
}

fn is_group(jid: &str) -> bool {
    jid.ends_with(GROUP_SERVER)
}

fn fallback_label(jid: &str) -> String {
    if is_group(jid) {
        return "Unnamed group".into();
    }
    let user = jid.split('@').next().unwrap_or(jid);
    let user = user.split(':').next().unwrap_or(user);
    if !user.is_empty() && user.chars().all(|ch| ch.is_ascii_digit()) {
        format!("+{user}")
    } else {
        "Unknown contact".into()
    }
}

fn remember_name(names: &mut HashMap<String, String>, message: &WaMessage) {
    if message.from_me {
        return;
    }
    let (Some(jid), Some(name)) = (&message.sender_jid, &message.sender_name) else {
        return;
    };
    let name = name.trim();
    if !name.is_empty() {
        names.insert(jid.clone(), name.to_string());
    }
}

fn insert_message(
    messages: &mut Vec<WaMessage>,
    page_floor: &mut Option<(i64, String)>,
    mut message: WaMessage,
) {
    // Freeze the capped window the first time an older page arrives.
    // A later live row must not move that mark, or the page just fetched
    // is the new oldest and the cap deletes it.
    let previous = messages.first().map(|row| (row.timestamp, row.id.clone()));
    let incoming = (message.timestamp, message.id.clone());
    let replacing = messages.iter().any(|row| row.id == message.id);
    if let Some(existing) = messages.iter_mut().find(|row| row.id == message.id) {
        // A refresh with no reactions keeps the reactions the cache counted.
        if message.reactions.is_empty() {
            message.reactions = std::mem::take(&mut existing.reactions);
        }
        *existing = message;
    } else {
        messages.push(message);
    }
    messages.sort_by(|a, b| a.timestamp.cmp(&b.timestamp).then_with(|| a.id.cmp(&b.id)));
    if !replacing
        && page_floor.is_none()
        && let Some(oldest) = previous.clone()
        && (incoming.0, incoming.1.as_str()) < (oldest.0, oldest.1.as_str())
    {
        *page_floor = Some(oldest);
    }
    let floor = page_floor.clone().or(previous);
    // Drop the oldest rows first, but never a local `pending:` row. A pending
    // or failed send must stay until its result or a resend resolves it.
    // Rows older than `page_floor` are a fetched page and stay.
    let mut excess = messages.len().saturating_sub(MESSAGES_PER_CHAT);
    if excess > 0 {
        messages.retain(|row| {
            if excess == 0 || row.id.starts_with(PENDING_PREFIX) {
                return true;
            }
            if floor.as_ref().is_some_and(|(timestamp, id)| {
                (row.timestamp, row.id.as_str()) < (*timestamp, id.as_str())
            }) {
                return true;
            }
            excess -= 1;
            false
        });
    }
}

fn preview(body: &str) -> String {
    let line = body.lines().next().unwrap_or_default();
    let mut out: String = line.chars().take(PREVIEW_CHARS).collect();
    if line.chars().count() > PREVIEW_CHARS {
        out.push('…');
    }
    out
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// A cache file of an older build has no reactions and no replies. It
    /// still reads. Each person counts once, and a new emoji of the same
    /// person takes the place of the old one.
    #[test]
    fn reactions_count_each_person_once_and_old_saves_still_read() {
        let old = r#"{"id":"a","from_me":false,"sender_name":null,"sender_jid":null,"body":"hi","timestamp":1}"#;
        let saved: SavedMessage = serde_json::from_str(old).expect("old line");
        assert!(saved.reactions.is_empty() && saved.reply_to.is_none());

        let mut reactions = Vec::new();
        set_reaction(&mut reactions, "b@s.whatsapp.net", "👍");
        set_reaction(&mut reactions, ME, "👍");
        set_reaction(&mut reactions, "c@s.whatsapp.net", "❤️");
        set_reaction(&mut reactions, "c@s.whatsapp.net", "👍");
        let counts = reaction_counts(&reactions);
        assert_eq!(counts.len(), 1);
        assert_eq!((counts[0].count, counts[0].mine), (3, true));
        set_reaction(&mut reactions, ME, "");
        assert_eq!(
            reaction_counts(&reactions)[0].count,
            2,
            "an empty emoji removes"
        );
    }

    pub(in crate::whatsapp) fn message(chat: &str, id: &str, body: &str, ts: i64) -> WaMessage {
        WaMessage {
            chat_jid: chat.into(),
            id: id.into(),
            from_me: false,
            sender_name: Some("Ana".into()),
            sender_jid: Some(chat.into()),
            body: body.into(),
            timestamp: ts,
            sticker: None,
            voice: None,
            reactions: Vec::new(),
            reply_to: None,
        }
    }

    pub(in crate::whatsapp) fn sticker_message(chat: &str, id: &str, ts: i64) -> WaMessage {
        WaMessage {
            sticker: Some(Sticker {
                width: 512,
                height: 256,
                media: MediaRef::new(id.to_string()),
                photo: false,
            }),
            ..message(chat, id, "[sticker]", ts)
        }
    }

    fn received_id(events: &[AdapterEvent], id: &str) -> Option<ChatMessage> {
        events.iter().find_map(|event| match event {
            AdapterEvent::MessageReceived { message } if message.id == id => Some(message.clone()),
            _ => None,
        })
    }

    /// A 1x1 PNG.
    pub(in crate::whatsapp) const PIXEL_PNG: [u8; 67] = [
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    fn photo_message(chat: &str, id: &str, body: &str) -> WaMessage {
        WaMessage {
            sticker: Some(Sticker {
                width: 0,
                height: 0,
                media: MediaRef::new(id.to_string()),
                photo: true,
            }),
            ..message(chat, id, body, 1)
        }
    }

    /// A photo row shows its label or its caption first. With its image, a
    /// caption stays the body, the label goes, and the photo has the size of
    /// its image.
    #[test]
    fn a_photo_keeps_its_caption_and_drops_its_label() {
        let chat = "15550111@s.whatsapp.net";
        let mut inbox = Inbox::default();
        let events = inbox.apply_messages(vec![
            photo_message(chat, "p1", "[photo]"),
            photo_message(chat, "p2", "at the lake"),
        ]);
        assert_eq!(inbox.media_requests(&events, true).len(), 2);
        for (id, body) in [("p1", ""), ("p2", "at the lake")] {
            let event = inbox
                .finish_media(chat, id, Some(PIXEL_PNG.to_vec()))
                .expect("the row again");
            let AdapterEvent::MessageReceived { message } = event else {
                panic!("not a message row: {event:?}");
            };
            assert_eq!(message.body, body, "{id}");
            let photo = message.photo.expect("photo");
            assert_eq!((photo.width, photo.height), (1, 1), "{id}");
        }
    }

    /// A sticker row shows "[sticker]" first and asks for its image one
    /// time. With the image, the row comes again with the photo and no label.
    /// The chat list keeps "[sticker]" as the preview.
    #[test]
    fn a_sticker_shows_its_image_after_the_download() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        let events = inbox.apply_messages(vec![sticker_message(chat, "s1", 1)]);
        let row = received_id(&events, "s1").expect("sticker row");
        assert_eq!(row.body, "[sticker]");
        assert!(row.photo.is_none());
        let requests = inbox.media_requests(&events, true);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].id, "s1");
        assert!(
            inbox.media_requests(&events, true).is_empty(),
            "a running download does not ask again"
        );

        let event = inbox
            .finish_media(chat, "s1", Some(vec![1, 2, 3]))
            .expect("the row again");
        let AdapterEvent::MessageReceived { message } = event else {
            panic!("not a message row: {event:?}");
        };
        assert_eq!(message.body, "");
        assert_eq!(message.arrival, Arrival::History);
        let photo = message.photo.expect("photo");
        assert_eq!((photo.width, photo.height), (160, 80));
        assert_eq!(
            photo.sizes[0].bytes.as_ref().map(ImageBytes::as_slice),
            Some([1, 2, 3].as_slice())
        );
        // An open of the chat sends the image with the row.
        let opened = inbox.open_chat(chat).expect("open");
        assert!(received_id(&opened, "s1").expect("row").photo.is_some());
        assert!(inbox.media_requests(&opened, true).is_empty());
        let preview = upserts(&opened)[0].preview.clone();
        assert_eq!(preview, "[sticker]");
    }

    /// A failed or too large download keeps the label and does not ask again.
    /// A text message never asks.
    #[test]
    fn a_failed_sticker_download_keeps_the_label() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        let events = inbox.apply_messages(vec![
            sticker_message(chat, "s1", 1),
            sticker_message(chat, "s2", 2),
            message(chat, "t1", "hi", 3),
        ]);
        let requests = inbox.media_requests(&events, true);
        assert_eq!(requests.len(), 2);
        assert_eq!(inbox.finish_media(chat, "s1", None), None);
        assert_eq!(
            inbox.finish_media(chat, "s2", Some(vec![0; MEDIA_MAX_BYTES + 1])),
            None
        );
        let opened = inbox.open_chat(chat).expect("open");
        assert_eq!(received_id(&opened, "s1").expect("row").body, "[sticker]");
        assert!(inbox.media_requests(&opened, true).is_empty());
        // A result after a reset is dropped.
        inbox.clear();
        assert_eq!(inbox.finish_media(chat, "s1", Some(vec![1])), None);
    }

    #[test]
    fn the_sticker_limit_is_the_wire_limit() {
        assert_eq!(MEDIA_MAX_BYTES, thinwire_ipc::WirePhoto::MAX_BYTES);
    }

    #[test]
    fn a_sticker_keeps_its_shape_at_the_thread_size() {
        assert_eq!(sticker_side(512, 512), (160, 160));
        assert_eq!(sticker_side(256, 512), (80, 160));
        assert_eq!(sticker_side(0, 0), (160, 160));
        assert_eq!(sticker_side(1, 4000), (1, 160));
    }

    fn upserts(events: &[AdapterEvent]) -> Vec<&Conversation> {
        events
            .iter()
            .filter_map(|event| match event {
                AdapterEvent::ConversationUpsert { conversation } => Some(conversation),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn mute_values_follow_whatsapp_app_state() {
        let now = 1_800_000_000_000;
        assert_eq!(Mute::from_action(Some(false), Some(-1)), Mute::Off);
        assert_eq!(
            Mute::from_action(Some(true), Some(0)),
            Mute::Off,
            "0 is unmuted"
        );
        assert_eq!(Mute::from_action(Some(true), Some(-1)), Mute::Forever);
        assert_eq!(Mute::from_action(Some(true), None), Mute::Forever);
        assert_eq!(
            Mute::from_action(Some(true), Some(now + 60_000)),
            Mute::UntilMs(now + 60_000)
        );
        assert_eq!(Mute::from_history(None), Mute::Off);
        assert_eq!(Mute::from_history(Some(0)), Mute::Off);
        assert_eq!(Mute::from_history(Some(u64::MAX)), Mute::Forever);
        // A small value is in seconds.
        assert_eq!(
            Mute::from_history(Some(1_800_000_060)),
            Mute::UntilMs(1_800_000_060_000)
        );
        assert!(Mute::Forever.is_on(now));
        assert!(Mute::UntilMs(now + 1).is_on(now));
        assert!(!Mute::UntilMs(now).is_on(now), "the mute ended");
        assert!(!Mute::Off.is_on(now));
    }

    #[test]
    fn a_chat_muted_on_the_phone_is_a_muted_row() {
        const CHAT: &str = "111@s.whatsapp.net";
        let mut inbox = Inbox::default();
        let rows = inbox.apply_history(
            vec![HistoryChat {
                mute: Mute::Forever,
                jid: CHAT.into(),
                name: None,
                unread: 0,
                timestamp: 10,
                messages: vec![message(CHAT, "a", "hi", 10)],
            }],
            Vec::new(),
        );
        assert!(upserts(&rows)[0].muted, "from the history sync");

        // The phone unmutes it: a live change.
        let row = inbox.apply_mute(CHAT, Mute::Off).expect("row");
        assert!(!upserts(&[row])[0].muted, "unmuted");
        // An older history chunk does not mute it again.
        let rows = inbox.apply_history(
            vec![HistoryChat {
                mute: Mute::Forever,
                jid: CHAT.into(),
                name: None,
                unread: 0,
                timestamp: 5,
                messages: Vec::new(),
            }],
            Vec::new(),
        );
        assert!(!upserts(&rows)[0].muted, "the live change wins");

        // A mute that ended by its time: the row comes before the message.
        inbox.apply_mute(CHAT, Mute::UntilMs(1));
        let events = inbox.apply_messages(vec![message(CHAT, "b", "back", 20)]);
        let first_row = events
            .iter()
            .position(|event| matches!(event, AdapterEvent::ConversationUpsert { .. }))
            .expect("row");
        let the_message = events
            .iter()
            .position(|event| matches!(event, AdapterEvent::MessageReceived { .. }))
            .expect("message");
        assert!(first_row < the_message);
        assert!(!upserts(&events)[0].muted);
    }

    /// Unread messages of the rows that are not muted: the shell's window
    /// title count (`Snapshot::unread_total`).
    fn title_count(rows: &[&Conversation]) -> u32 {
        rows.iter()
            .filter(|row| !row.muted)
            .map(|row| row.unread)
            .sum()
    }

    fn mute_chunk(jid: &str, mute: Mute) -> Vec<HistoryChat> {
        vec![HistoryChat {
            jid: jid.into(),
            name: None,
            unread: 0,
            timestamp: 10,
            messages: Vec::new(),
            mute,
        }]
    }

    /// #168 item 12: after a reconnect, a history value refreshes the mute.
    /// A live change of the same connection still wins over a later chunk.
    #[test]
    fn a_reconnect_lets_a_history_mute_refresh_the_row() {
        const CHAT: &str = "111@s.whatsapp.net";
        let mut inbox = Inbox::default();
        inbox.apply_mute(CHAT, Mute::Forever);
        let rows = inbox.apply_history(mute_chunk(CHAT, Mute::Off), Vec::new());
        assert!(
            upserts(&rows)[0].muted,
            "the live change of this connection wins"
        );

        inbox.new_connection();
        let rows = inbox.apply_history(mute_chunk(CHAT, Mute::Off), Vec::new());
        assert!(
            !upserts(&rows)[0].muted,
            "the phone unmuted it while offline"
        );

        inbox.apply_mute(CHAT, Mute::Forever);
        let rows = inbox.apply_history(mute_chunk(CHAT, Mute::Off), Vec::new());
        assert!(upserts(&rows)[0].muted, "a new live change wins again");
    }

    /// #168 item 13: a timed mute becomes Off when its end passes. Its row
    /// comes once, not muted, so its unread counts in the title again.
    #[test]
    fn an_expired_mute_turns_off_and_counts_in_the_title_again() {
        const CHAT: &str = "111@s.whatsapp.net";
        let mut inbox = Inbox::default();
        let end = now_ms() + 60_000;
        inbox.apply_mute(CHAT, Mute::UntilMs(end));
        let rows = inbox.apply_messages(vec![
            message(CHAT, "a", "one", 1),
            message(CHAT, "b", "two", 2),
        ]);
        let last = upserts(&rows).into_iter().last().expect("row");
        assert_eq!(last.unread, 2);
        assert_eq!(title_count(&[last]), 0, "a muted chat is not in the title");
        assert_eq!(inbox.next_mute_end(), Some(end));
        assert!(inbox.expire_mutes(end - 1).is_empty(), "not yet");

        let rows = inbox.expire_mutes(end);
        assert_eq!(upserts(&rows).len(), 1);
        assert_eq!(title_count(&upserts(&rows)), 2, "its unread counts again");
        assert_eq!(inbox.chats[CHAT].mute, Mute::Off);
        assert_eq!(inbox.next_mute_end(), None);
        assert!(inbox.expire_mutes(end + 1).is_empty(), "one row only");
    }

    /// #168 item 13: a message after the mute end sends the extra row once.
    #[test]
    fn an_ended_mute_sends_its_extra_row_once() {
        const CHAT: &str = "111@s.whatsapp.net";
        let mut inbox = Inbox::default();
        inbox.apply_mute(CHAT, Mute::UntilMs(1));
        let first = inbox.apply_messages(vec![message(CHAT, "a", "one", 10)]);
        assert_eq!(
            upserts(&first).len(),
            2,
            "a row before the message and after it"
        );
        let second = inbox.apply_messages(vec![message(CHAT, "b", "two", 11)]);
        assert_eq!(upserts(&second).len(), 1, "no extra row");
    }

    #[test]
    fn history_builds_titles_previews_and_order() {
        let mut inbox = Inbox::default();
        let events = inbox.apply_history(
            vec![
                HistoryChat {
                    mute: Default::default(),
                    jid: "111@s.whatsapp.net".into(),
                    name: None,
                    unread: 2,
                    timestamp: 10,
                    messages: vec![message("111@s.whatsapp.net", "a", "hi\nsecond line", 10)],
                },
                HistoryChat {
                    mute: Default::default(),
                    jid: "g1@g.us".into(),
                    name: Some("Family".into()),
                    unread: 0,
                    timestamp: 20,
                    messages: Vec::new(),
                },
                HistoryChat {
                    mute: Default::default(),
                    jid: "222@s.whatsapp.net".into(),
                    name: None,
                    unread: 0,
                    timestamp: 5,
                    messages: Vec::new(),
                },
            ],
            vec![("222@s.whatsapp.net".into(), "Bo".into())],
        );
        let rows = upserts(&events);
        assert_eq!(rows.len(), 3);
        let by_id = |id: &str| *rows.iter().find(|row| row.id == id).expect("row");
        let ana = by_id("whatsapp:111@s.whatsapp.net");
        assert_eq!(ana.title, "Ana");
        assert_eq!(ana.preview, "hi");
        assert_eq!(ana.unread, 2);
        assert_eq!(ana.order, 10);
        let family = by_id("whatsapp:g1@g.us");
        assert_eq!(family.title, "Family");
        assert_eq!(family.participant, "Group");
        assert_eq!(by_id("whatsapp:222@s.whatsapp.net").title, "Bo");

        let page = inbox.chat_page();
        let order: Vec<&str> = upserts(&page).iter().map(|row| row.id.as_str()).collect();
        assert_eq!(
            order,
            vec![
                "whatsapp:g1@g.us",
                "whatsapp:111@s.whatsapp.net",
                "whatsapp:222@s.whatsapp.net",
            ]
        );
    }

    #[test]
    fn unknown_dm_falls_back_to_the_number() {
        let mut inbox = Inbox::default();
        let mut row = message("333@s.whatsapp.net", "x", "yo", 1);
        row.sender_name = None;
        let events = inbox.apply_messages(vec![row]);
        assert_eq!(upserts(&events)[0].title, "+333");
        let lid = inbox.apply_messages(vec![WaMessage {
            sender_name: None,
            ..message("abc@lid", "y", "yo", 1)
        }]);
        assert_eq!(upserts(&lid)[0].title, "Unknown contact");
    }

    #[test]
    fn open_chat_resets_unread_and_returns_latest_messages() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        let history: Vec<WaMessage> = (0..(OPEN_CHAT_MESSAGES as i64 + 5))
            .map(|n| message(chat, &format!("m{n:03}"), &format!("body {n}"), n))
            .collect();
        inbox.apply_history(
            vec![HistoryChat {
                mute: Default::default(),
                jid: chat.into(),
                name: None,
                unread: 4,
                timestamp: 0,
                messages: history,
            }],
            Vec::new(),
        );
        let events = inbox.open_chat(chat).expect("known chat");
        assert_eq!(upserts(&events)[0].unread, 0);
        let bodies: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                AdapterEvent::MessageReceived { message } => Some(message.id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(bodies.len(), OPEN_CHAT_MESSAGES);
        assert_eq!(bodies.first().copied(), Some("m005"));
        assert_eq!(bodies.last().copied(), Some("m054"));
        assert!(inbox.open_chat("nope@s.whatsapp.net").is_none());

        // Inbound traffic in the open chat does not raise unread.
        let live = inbox.apply_messages(vec![message(chat, "live", "new", 100)]);
        assert_eq!(upserts(&live)[0].unread, 0);
        let other = inbox.apply_messages(vec![message("9@s.whatsapp.net", "o", "hey", 1)]);
        assert_eq!(upserts(&other)[0].unread, 1);
    }

    #[test]
    fn duplicate_live_messages_do_not_double_count() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        inbox.apply_messages(vec![message(chat, "a", "one", 1)]);
        let events = inbox.apply_messages(vec![message(chat, "a", "one", 1)]);
        assert_eq!(upserts(&events)[0].unread, 1);
    }

    /// A pushed row can notify. A repeat, an opened history page, and a
    /// pending send stay `History` (#32).
    #[test]
    fn a_pushed_message_is_live_and_a_replay_is_history() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        let first = inbox.apply_messages(vec![message(chat, "live", "new", 100)]);
        assert_eq!(received(&first).arrival, Arrival::Live);
        assert!(!upserts(&first)[0].muted);
        let again = inbox.apply_messages(vec![message(chat, "live", "new", 100)]);
        assert_eq!(received(&again).arrival, Arrival::History);
        let opened = inbox.open_chat(chat).expect("known chat");
        assert!(opened.iter().all(|event| match event {
            AdapterEvent::MessageReceived { message } => message.arrival == Arrival::History,
            _ => true,
        }));
        let (_, pending, _) = inbox.begin_send(chat, "out", 101);
        assert_eq!(
            received(std::slice::from_ref(&pending)).arrival,
            Arrival::History
        );
    }

    fn received(events: &[AdapterEvent]) -> &ChatMessage {
        events
            .iter()
            .find_map(|event| match event {
                AdapterEvent::MessageReceived { message } => Some(message),
                _ => None,
            })
            .expect("message")
    }

    #[test]
    fn send_pending_is_replaced_or_removed() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        let (pending, shown, upsert) = inbox.begin_send(chat, "hello", 50);
        match upsert {
            Some(AdapterEvent::ConversationUpsert { conversation }) => {
                assert_eq!(conversation.preview, "hello");
                assert_eq!(conversation.order, 50);
            }
            other => panic!("the send must update the chat row: {other:?}"),
        }
        match shown {
            AdapterEvent::MessageReceived { message } => {
                assert!(message.outbound);
                assert_eq!(message.sender, "You");
                assert_eq!(message.id, pending);
            }
            other => panic!("unexpected {other:?}"),
        }
        let events = inbox.confirm_send(chat, &pending, "SRV1".into());
        assert!(events.iter().any(|event| matches!(
            event,
            AdapterEvent::MessageReplaced { old_id, message, .. }
                if old_id == &pending && message.id == "SRV1"
        )));

        let (failed, _, _) = inbox.begin_send(chat, "again", 60);
        let events = inbox.fail_send(chat, &failed);
        assert_eq!(
            events,
            vec![delivery_event(chat, &failed, Delivery::Failed)]
        );
        let open = inbox.open_chat(chat).expect("chat");
        let rows: Vec<(String, Delivery)> = open
            .iter()
            .filter_map(|event| match event {
                AdapterEvent::MessageReceived { message } => {
                    Some((message.id.clone(), message.delivery))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                ("SRV1".to_string(), Delivery::Sent),
                (failed.clone(), Delivery::Failed)
            ]
        );

        let (body, event) = inbox.retry_send(chat, &failed).expect("failed row");
        assert_eq!(body, "again");
        assert_eq!(event, delivery_event(chat, &failed, Delivery::Pending));
        assert!(
            inbox.retry_send(chat, &failed).is_none(),
            "only a failed row"
        );
        assert!(inbox.retry_send(chat, "SRV1").is_none());
    }

    #[test]
    fn confirm_after_echo_keeps_one_copy() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        let (pending, _, _) = inbox.begin_send(chat, "hello", 50);
        let mut echo = message(chat, "SRV1", "hello", 50);
        echo.from_me = true;
        inbox.apply_messages(vec![echo]);
        inbox.confirm_send(chat, &pending, "SRV1".into());
        let open = inbox.open_chat(chat).expect("chat");
        let count = open
            .iter()
            .filter(|event| matches!(event, AdapterEvent::MessageReceived { .. }))
            .count();
        assert_eq!(count, 1);
    }

    /// Codex r4093058047: a chat the user left counts unread again.
    #[test]
    fn leaving_a_chat_counts_its_messages_unread_again() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        inbox.apply_messages(vec![message(chat, "a", "one", 1)]);
        let read = inbox.view(Some(chat)).expect("marks the chat read");
        assert!(matches!(
            read,
            AdapterEvent::ConversationUpsert { conversation } if conversation.unread == 0
        ));
        let events = inbox.apply_messages(vec![message(chat, "b", "two", 2)]);
        assert_eq!(upserts(&events)[0].unread, 0, "the viewed chat stays read");

        assert!(inbox.view(None).is_none());
        let events = inbox.apply_messages(vec![message(chat, "c", "three", 3)]);
        assert_eq!(upserts(&events)[0].unread, 1, "a left chat counts again");

        // Viewing another chat also leaves this one.
        inbox.view(Some("222@s.whatsapp.net"));
        let events = inbox.apply_messages(vec![message(chat, "d", "four", 4)]);
        assert_eq!(upserts(&events)[0].unread, 2);
    }

    /// #98 (Codex r4104217030): a chat read here and then left stays read
    /// when a later history chunk repeats the phone's older unread count.
    /// Only inbound messages newer than the read count.
    #[test]
    fn a_chat_read_here_stays_read_after_a_later_chunk() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        let chunk = |unread: u32, messages: Vec<WaMessage>| HistoryChat {
            mute: Default::default(),
            jid: chat.into(),
            name: None,
            unread,
            timestamp: 10,
            messages,
        };
        inbox.apply_history(
            vec![chunk(3, vec![message(chat, "a", "one", 10)])],
            Vec::new(),
        );
        assert!(inbox.view(Some(chat)).is_some(), "viewing marks it read");
        inbox.view(None);

        // The phone still reports 3 unread: the chat stays read.
        let events = inbox.apply_history(vec![chunk(3, Vec::new())], Vec::new());
        assert_eq!(upserts(&events)[0].unread, 0);

        // A newer inbound message in a chunk counts; an own message does not.
        let mut own = message(chat, "c", "mine", 12);
        own.from_me = true;
        let events = inbox.apply_history(
            vec![chunk(5, vec![message(chat, "b", "new", 11), own])],
            Vec::new(),
        );
        assert_eq!(upserts(&events)[0].unread, 1);

        // A chat never read here still takes the phone's count.
        let other = "222@s.whatsapp.net";
        let events = inbox.apply_history(
            vec![HistoryChat {
                mute: Default::default(),
                jid: other.into(),
                name: None,
                unread: 4,
                timestamp: 1,
                messages: Vec::new(),
            }],
            Vec::new(),
        );
        assert_eq!(upserts(&events)[0].unread, 4);
    }

    /// Codex r4131089163: a chat read here counts every new inbound message,
    /// also past the 200-row cache and in the read second, and a repeat of
    /// a chunk does not count twice.
    #[test]
    fn unread_after_a_read_counts_events_not_cached_rows() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        let chunk = |messages: Vec<WaMessage>| HistoryChat {
            mute: Default::default(),
            jid: chat.into(),
            name: None,
            unread: 0,
            timestamp: 10,
            messages,
        };
        inbox.apply_history(
            vec![chunk(vec![message(chat, "seen", "one", 10)])],
            Vec::new(),
        );
        inbox.view(Some(chat));
        inbox.view(None);

        let flood: Vec<WaMessage> = (0..(MESSAGES_PER_CHAT + 50))
            .map(|n| {
                message(
                    chat,
                    &format!("n{n:04}"),
                    "new",
                    11 + i64::try_from(n).expect("small"),
                )
            })
            .collect();
        let events = inbox.apply_history(vec![chunk(flood.clone())], Vec::new());
        let total = u32::try_from(MESSAGES_PER_CHAT + 50).expect("small");
        assert_eq!(upserts(&events)[0].unread, total, "past the row cache");
        let events = inbox.apply_history(vec![chunk(flood)], Vec::new());
        assert_eq!(upserts(&events)[0].unread, total, "a repeat does not count");

        // The read second: a new message there counts; a seen one does not.
        let mut inbox = Inbox::default();
        inbox.apply_history(
            vec![chunk(vec![message(chat, "seen", "one", 10)])],
            Vec::new(),
        );
        inbox.view(Some(chat));
        inbox.view(None);
        let events = inbox.apply_history(
            vec![chunk(vec![
                message(chat, "seen", "one", 10),
                message(chat, "same-second", "two", 10),
            ])],
            Vec::new(),
        );
        assert_eq!(upserts(&events)[0].unread, 1);
    }

    /// Codex r4131622220: after a local read, a live redelivery of an id
    /// that left the capped cache does not count again.
    #[test]
    fn a_redelivered_live_message_counts_once_after_a_read() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        inbox.apply_messages(vec![message(chat, "seen", "one", 1)]);
        inbox.view(Some(chat));
        inbox.view(None);
        let live: Vec<WaMessage> = (0..(MESSAGES_PER_CHAT + 50))
            .map(|n| {
                message(
                    chat,
                    &format!("l{n:04}"),
                    "new",
                    2 + i64::try_from(n).expect("small"),
                )
            })
            .collect();
        let first = live[0].clone();
        inbox.apply_messages(live);
        let total = u32::try_from(MESSAGES_PER_CHAT + 50).expect("small");
        assert!(
            !inbox.chats[chat]
                .messages
                .iter()
                .any(|row| row.id == first.id),
            "the first live row left the cache"
        );
        let events = inbox.apply_messages(vec![first]);
        assert_eq!(
            upserts(&events)[0].unread,
            total,
            "a redelivery does not count"
        );
    }

    /// Codex r4132341446: a read keeps every seen inbound id, not only the
    /// ids of the read second. After newer rows evict an older read id, a
    /// live redelivery does not count. A new id in that second still does.
    #[test]
    fn a_read_id_evicted_from_the_cache_does_not_count_again() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        inbox.apply_messages(vec![
            message(chat, "old", "one", 1),
            message(chat, "newer", "two", 2),
        ]);
        inbox.view(Some(chat));
        let flood: Vec<WaMessage> = (0..MESSAGES_PER_CHAT)
            .map(|n| {
                message(
                    chat,
                    &format!("n{n:04}"),
                    "new",
                    3 + i64::try_from(n).expect("small"),
                )
            })
            .collect();
        inbox.apply_messages(flood);
        assert!(
            !inbox.chats[chat].messages.iter().any(|row| row.id == "old"),
            "the older row left the cache"
        );
        inbox.view(None);
        let events = inbox.apply_messages(vec![message(chat, "old", "one", 1)]);
        assert_eq!(upserts(&events)[0].unread, 0, "a redelivery does not count");
        let read_at = inbox.chats[chat].read_at.expect("read");
        let events = inbox.apply_messages(vec![message(chat, "fresh", "same second", read_at)]);
        assert_eq!(
            upserts(&events)[0].unread,
            1,
            "a new id in the read second counts"
        );
    }

    /// #158 review: a read drops handled ids older than the read point.
    /// 10,000 messages with a read after each keep the set tiny.
    #[test]
    fn many_reads_keep_handled_small() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        let mut largest = 0;
        for n in 0..10_000_i64 {
            inbox.apply_messages(vec![message(chat, &format!("m{n}"), "new", n + 1)]);
            inbox.view(Some(chat));
            inbox.view(None);
            largest = largest.max(inbox.chats[chat].handled.len());
        }
        assert!(largest <= 2, "a read drops older ids, got {largest}");
        assert_eq!(inbox.chats[chat].unread, 0);
    }

    /// #158 review: a chat read once and then ignored keeps `handled` below
    /// the cap.
    #[test]
    fn a_busy_chat_after_one_read_keeps_handled_below_the_cap() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        inbox.apply_messages(vec![message(chat, "seen", "one", 1)]);
        inbox.view(Some(chat));
        inbox.view(None);
        let flood: u32 = 10_000;
        for n in 0..flood {
            inbox.apply_messages(vec![message(
                chat,
                &format!("n{n:05}"),
                "new",
                2 + i64::from(n),
            )]);
            assert!(inbox.chats[chat].handled.len() <= HANDLED_PER_CHAT);
        }
        assert_eq!(inbox.chats[chat].unread, flood);
        assert!(!inbox.chats[chat].handled.contains_key("n00000"));
        let events =
            inbox.apply_messages(vec![message(chat, "fresh", "new", 2 + i64::from(flood))]);
        assert_eq!(upserts(&events)[0].unread, flood + 1);
    }

    /// #168 item 5: a cap trim keeps the read point. A late new message
    /// older than the ids that stay, but not older than the read, counts.
    #[test]
    fn a_cap_trim_keeps_the_read_point() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        inbox.apply_messages(vec![message(chat, "seen", "one", 1)]);
        inbox.view(Some(chat));
        inbox.view(None);
        let flood = u32::try_from(HANDLED_PER_CHAT + 1).expect("small");
        let messages: Vec<WaMessage> = (0..flood)
            .map(|n| message(chat, &format!("n{n:05}"), "new", 10 + i64::from(n)))
            .collect();
        inbox.apply_messages(messages);
        assert_eq!(
            inbox.chats[chat].read_at,
            Some(1),
            "the trim keeps the read point"
        );
        let events = inbox.apply_messages(vec![message(chat, "late", "late push", 5)]);
        assert_eq!(upserts(&events)[0].unread, flood + 1);
    }

    /// #168 item 6: a chunk time newer than its rows does not lift the read
    /// point. A message between them was not seen, so it counts.
    #[test]
    fn a_chunk_time_past_its_rows_does_not_lift_the_read_point() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        inbox.apply_history(
            vec![HistoryChat {
                mute: Default::default(),
                jid: chat.into(),
                name: None,
                unread: 0,
                timestamp: 100,
                messages: vec![message(chat, "a", "seen", 90)],
            }],
            Vec::new(),
        );
        inbox.view(Some(chat));
        inbox.view(None);
        assert_eq!(inbox.chats[chat].read_at, Some(90));
        let events = inbox.apply_messages(vec![message(chat, "b", "not seen", 95)]);
        assert_eq!(upserts(&events)[0].unread, 1);

        // With no row, the chunk time is the read point.
        let empty = "222@s.whatsapp.net";
        inbox.apply_history(
            vec![HistoryChat {
                mute: Default::default(),
                jid: empty.into(),
                name: None,
                unread: 0,
                timestamp: 50,
                messages: Vec::new(),
            }],
            Vec::new(),
        );
        inbox.view(Some(empty));
        inbox.view(None);
        assert_eq!(inbox.chats[empty].read_at, Some(50));
    }

    /// #168 item 7: an echo that wins over a pending send gives the chat the
    /// server time back, also when the local clock is ahead.
    #[test]
    fn an_echo_before_confirm_restores_the_chat_time() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        inbox.apply_messages(vec![message(chat, "a", "before", 60)]);
        let (pending, _, _) = inbox.begin_send(chat, "mine", 100);
        let mut echo = message(chat, "SRV1", "mine", 80);
        echo.from_me = true;
        inbox.apply_messages(vec![echo]);
        let events = inbox.confirm_send(chat, &pending, "SRV1".into());
        let row = upserts(&events)[0];
        assert_eq!(row.last_at, 80);
        assert_eq!(row.order, 80);
    }

    /// #158 review: messages with one equal time also keep `handled` below
    /// the cap. The oldest arrivals go first.
    #[test]
    fn equal_times_keep_handled_below_the_cap() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        inbox.apply_messages(vec![message(chat, "seen", "one", 0)]);
        inbox.view(Some(chat));
        inbox.view(None);
        let flood: u32 = 10_000;
        for n in 0..flood {
            inbox.apply_messages(vec![message(chat, &format!("n{n:05}"), "new", 0)]);
        }
        let record = &inbox.chats[chat];
        assert!(record.handled.len() <= HANDLED_PER_CHAT);
        assert!(
            !record.handled.contains_key("n00000"),
            "the oldest went first"
        );
        assert!(record.handled.contains_key("n09999"));
        assert_eq!(record.unread, flood);
    }

    /// #158 review: an id that cannot count does not go into `handled`.
    #[test]
    fn only_ids_that_can_count_are_handled() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        inbox.apply_messages(vec![message(chat, "a", "one", 1)]);
        inbox.view(Some(chat));
        inbox.view(None);
        let mut mine = message(chat, "mine", "sent on the phone", 2);
        mine.from_me = true;
        inbox.apply_messages(vec![mine]);
        assert!(!inbox.chats[chat].handled.contains_key("mine"));
    }

    /// #158 review: after a read, a message older than the read point does
    /// not count, live or from history. One rule for both paths.
    #[test]
    fn a_message_older_than_the_read_point_counts_on_neither_path() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        inbox.apply_messages(vec![message(chat, "a", "one", 10)]);
        inbox.view(Some(chat));
        inbox.view(None);
        let events = inbox.apply_messages(vec![message(chat, "late", "live", 5)]);
        assert_eq!(upserts(&events)[0].unread, 0);
        let events = inbox.apply_history(
            vec![HistoryChat {
                mute: Default::default(),
                jid: chat.into(),
                name: None,
                unread: 0,
                timestamp: 10,
                messages: vec![message(chat, "old", "history", 6)],
            }],
            Vec::new(),
        );
        assert_eq!(upserts(&events)[0].unread, 0);
        let events = inbox.apply_messages(vec![message(chat, "new", "live", 10)]);
        assert_eq!(
            upserts(&events)[0].unread,
            1,
            "the read second still counts"
        );
    }

    /// #158 review: a confirmed send with no echo leaves `optimistic` when
    /// its row leaves the cache.
    #[test]
    fn a_confirmed_send_without_an_echo_leaves_with_its_row() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        let (pending, _, _) = inbox.begin_send(chat, "first", 1);
        inbox.confirm_send(chat, &pending, "SRV1".into());
        let flood: Vec<WaMessage> = (0..MESSAGES_PER_CHAT)
            .map(|n| {
                message(
                    chat,
                    &format!("n{n:04}"),
                    "new",
                    2 + i64::try_from(n).expect("small"),
                )
            })
            .collect();
        inbox.apply_messages(flood);
        let (pending, _, _) = inbox.begin_send(chat, "second", 1_000);
        inbox.confirm_send(chat, &pending, "SRV2".into());
        let optimistic = &inbox.chats[chat].optimistic;
        assert!(!optimistic.contains("SRV1"), "its row left the cache");
        assert!(optimistic.contains("SRV2"));
    }

    /// #158 review: the read point is server time, not a local send time.
    /// A later inbound message with an earlier server time still counts.
    #[test]
    fn the_read_point_uses_server_time_not_a_local_send() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        inbox.apply_messages(vec![message(chat, "a", "one", 10)]);
        // A local send with a clock far ahead of the server's.
        let _ = inbox.begin_send(chat, "mine", 1_000);
        inbox.view(Some(chat));
        inbox.view(None);
        let events = inbox.apply_history(
            vec![HistoryChat {
                mute: Default::default(),
                jid: chat.into(),
                name: None,
                unread: 0,
                timestamp: 20,
                messages: vec![message(chat, "b", "server twenty", 20)],
            }],
            Vec::new(),
        );
        assert_eq!(upserts(&events)[0].unread, 1);
    }

    /// Codex r4132341439: `confirm_send` drops the `pending:` prefix and keeps
    /// the local clock. The read point must not use that clock. An inbound
    /// message between the last server time and the local time still counts.
    #[test]
    fn a_confirmed_local_send_does_not_move_the_read_point() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        inbox.apply_messages(vec![message(chat, "a", "one", 10)]);
        let (pending, _, _) = inbox.begin_send(chat, "mine", 1_000);
        inbox.confirm_send(chat, &pending, "SRV1".into());
        let record = inbox.chats.get(chat).expect("chat");
        assert!(
            record.messages.iter().any(|row| row.id == "SRV1"),
            "the row is no longer pending"
        );
        assert!(record.messages.iter().all(|row| row.id != pending));
        inbox.view(Some(chat));
        inbox.view(None);
        let events = inbox.apply_history(
            vec![HistoryChat {
                mute: Default::default(),
                jid: chat.into(),
                name: None,
                unread: 0,
                timestamp: 20,
                messages: vec![message(chat, "b", "server twenty", 20)],
            }],
            Vec::new(),
        );
        assert_eq!(upserts(&events)[0].unread, 1);

        // A server copy replaces the local clock. The read point can move
        // to that time, and a message before it does not count.
        let mut echo = message(chat, "SRV1", "mine", 30);
        echo.from_me = true;
        inbox.apply_messages(vec![echo]);
        inbox.view(Some(chat));
        inbox.view(None);
        let events = inbox.apply_history(
            vec![HistoryChat {
                mute: Default::default(),
                jid: chat.into(),
                name: None,
                unread: 0,
                timestamp: 40,
                messages: vec![
                    message(chat, "c", "before the echo", 25),
                    message(chat, "d", "after the echo", 40),
                ],
            }],
            Vec::new(),
        );
        assert_eq!(upserts(&events)[0].unread, 1);
    }

    /// Codex r4132803796: an echo under the server id can arrive before
    /// confirm, with a clock ahead of the local send. The row that survives
    /// keeps that server clock, so opening the chat does not take the local
    /// clock as the read point. A later message between the two clocks does
    /// not count.
    #[test]
    fn an_echo_before_confirm_keeps_the_server_clock() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        inbox.apply_messages(vec![message(chat, "a", "one", 40)]);
        let local = 50;
        let server = 80;
        let (pending, _, _) = inbox.begin_send(chat, "mine", local);
        let mut echo = message(chat, "SRV1", "mine", server);
        echo.from_me = true;
        inbox.apply_messages(vec![echo]);
        let events = inbox.confirm_send(chat, &pending, "SRV1".into());
        assert!(events.iter().any(|event| matches!(
            event,
            AdapterEvent::MessageReplaced { old_id, message, .. }
                if old_id == &pending && message.id == "SRV1" && message.sent_at == server
        )));

        let record = inbox.chats.get(chat).expect("chat");
        let rows: Vec<_> = record
            .messages
            .iter()
            .filter(|row| row.id == "SRV1")
            .collect();
        assert_eq!(rows.len(), 1, "the echo and the pending row are one");
        assert_eq!(rows[0].timestamp, server, "the server clock survives");
        assert!(
            !record.optimistic.contains("SRV1"),
            "a server-stamped row is not a local clock"
        );
        assert!(record.messages.iter().all(|row| row.id != pending));

        inbox.view(Some(chat));
        inbox.view(None);
        assert_eq!(
            inbox.chats[chat].read_at,
            Some(server),
            "the read point is the server clock"
        );
        let events = inbox.apply_history(
            vec![HistoryChat {
                mute: Default::default(),
                jid: chat.into(),
                name: None,
                unread: 0,
                timestamp: server,
                messages: vec![
                    message(chat, "between", "after the local clock", local + 20),
                    message(chat, "after", "after the server clock", server + 10),
                ],
            }],
            Vec::new(),
        );
        assert_eq!(
            upserts(&events)[0].unread,
            1,
            "only the message after the server clock counts"
        );
    }

    /// Codex r4131089177: a dropped send gives the chat its time back.
    #[test]
    fn a_dropped_send_restores_the_chat_time() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        inbox.apply_history(
            vec![HistoryChat {
                mute: Default::default(),
                jid: chat.into(),
                name: None,
                unread: 0,
                timestamp: 5,
                messages: vec![message(chat, "a", "before", 5)],
            }],
            Vec::new(),
        );
        let (pending, _, _) = inbox.begin_send(chat, "failed", 100);
        let events = inbox.drop_send(chat, &pending);
        let row = upserts(&events)[0];
        assert_eq!(row.order, 5);
        assert_eq!(row.last_at, 5);
        assert_eq!(row.preview, "before");

        // With no message left, the history time stays.
        let other = "222@s.whatsapp.net";
        inbox.apply_history(
            vec![HistoryChat {
                mute: Default::default(),
                jid: other.into(),
                name: None,
                unread: 0,
                timestamp: 7,
                messages: Vec::new(),
            }],
            Vec::new(),
        );
        let (pending, _, _) = inbox.begin_send(other, "failed", 100);
        let events = inbox.drop_send(other, &pending);
        assert_eq!(upserts(&events)[0].order, 7);
    }

    /// Messages that arrive in the open chat move the read point, so leaving
    /// the chat later keeps them read.
    #[test]
    fn messages_seen_in_the_open_chat_stay_read_after_leaving() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        inbox.apply_messages(vec![message(chat, "a", "one", 1)]);
        inbox.view(Some(chat));
        inbox.apply_messages(vec![message(chat, "b", "two", 2)]);
        inbox.view(Some("222@s.whatsapp.net"));
        let events = inbox.apply_history(
            vec![HistoryChat {
                mute: Default::default(),
                jid: chat.into(),
                name: None,
                unread: 2,
                timestamp: 2,
                messages: Vec::new(),
            }],
            Vec::new(),
        );
        assert_eq!(upserts(&events)[0].unread, 0);
    }

    /// Codex r4103183211: a later history chunk keeps live unread increments.
    #[test]
    fn later_history_chunk_keeps_live_unread() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        let chunk = |unread: u32| HistoryChat {
            mute: Default::default(),
            jid: chat.into(),
            name: None,
            unread,
            timestamp: 1,
            messages: Vec::new(),
        };
        inbox.apply_history(vec![chunk(0)], Vec::new());
        inbox.apply_messages(vec![message(chat, "live", "hi", 5)]);
        let events = inbox.apply_history(vec![chunk(0)], Vec::new());
        assert_eq!(
            upserts(&events)[0].unread,
            1,
            "the live message stays unread"
        );
        let events = inbox.apply_history(vec![chunk(4)], Vec::new());
        assert_eq!(upserts(&events)[0].unread, 4, "a higher server count wins");
    }

    /// Codex r4093606402: a later history chunk keeps the open chat read.
    #[test]
    fn later_history_chunk_keeps_the_open_chat_read() {
        let mut inbox = Inbox::default();
        let chunk = |jid: &str, unread: u32| HistoryChat {
            mute: Default::default(),
            jid: jid.into(),
            name: None,
            unread,
            timestamp: 1,
            messages: Vec::new(),
        };
        let open = "111@s.whatsapp.net";
        let other = "222@s.whatsapp.net";
        inbox.apply_history(vec![chunk(open, 3), chunk(other, 2)], Vec::new());
        inbox.open_chat(open).expect("chat");
        let events = inbox.apply_history(vec![chunk(open, 5), chunk(other, 4)], Vec::new());
        let unread = |id: &str| {
            upserts(&events)
                .into_iter()
                .find(|row| row.id == conversation_id(id))
                .expect("row")
                .unread
        };
        assert_eq!(unread(open), 0, "the open chat stays read");
        assert_eq!(unread(other), 4, "other chats take the server count");
    }

    /// Codex r4093471553: a busy chat must not evict an unresolved send.
    #[test]
    fn history_trim_keeps_pending_and_failed_sends() {
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        let (pending, _, _) = inbox.begin_send(chat, "in flight", 1);
        let (failed, _, _) = inbox.begin_send(chat, "failed", 2);
        inbox.fail_send(chat, &failed);
        let flood: Vec<WaMessage> = (0..(MESSAGES_PER_CHAT as i64 + 50))
            .map(|n| message(chat, &format!("m{n:04}"), "busy", 10 + n))
            .collect();
        inbox.apply_messages(flood);

        let events = inbox.confirm_send(chat, &pending, "SRV1".into());
        assert!(events.iter().any(|event| matches!(
            event,
            AdapterEvent::MessageReplaced { old_id, .. } if old_id == &pending
        )));
        assert!(
            inbox.retry_send(chat, &failed).is_some(),
            "failed row is retryable"
        );
        let record = inbox.chats.get(chat).expect("chat");
        assert!(record.messages.len() <= MESSAGES_PER_CHAT + 2);
        assert!(
            !record.messages.iter().any(|row| row.id == "m0000"),
            "the oldest history row is evicted"
        );
    }

    #[test]
    fn conversation_ids_round_trip_and_reject_placeholders() {
        assert_eq!(
            parse_conversation_id(&conversation_id("111@s.whatsapp.net")),
            Some("111@s.whatsapp.net")
        );
        assert_eq!(parse_conversation_id("whatsapp:placeholder"), None);
        assert_eq!(parse_conversation_id("telegram:1"), None);
        assert_eq!(parse_conversation_id("whatsapp:@g.us"), None);
        assert_eq!(parse_conversation_id("whatsapp:1 2@g.us"), None);
    }

    #[test]
    fn status_broadcast_and_channels_stay_out() {
        let mut inbox = Inbox::default();
        let events = inbox.apply_messages(vec![
            message("status@broadcast", "s", "story", 1),
            message("123@newsletter", "n", "channel", 1),
            message("1@broadcast", "b", "list", 1),
        ]);
        assert!(events.is_empty());
        assert!(inbox.chat_page().is_empty());
    }

    /// Codex r4103855629: chats past the first page stay reachable.
    #[test]
    fn chat_list_pages_past_the_first_page() {
        let mut inbox = Inbox::default();
        let total = CHAT_PAGE * 2 + 50;
        let chats: Vec<HistoryChat> = (0..total)
            .map(|n| HistoryChat {
                mute: Default::default(),
                jid: format!("{n}@s.whatsapp.net"),
                name: None,
                unread: 0,
                timestamp: i64::try_from(n).expect("small"),
                messages: Vec::new(),
            })
            .collect();
        inbox.apply_history(chats, Vec::new());
        let mut seen = std::collections::HashSet::new();
        let first = inbox.chat_page();
        assert_eq!(first.len(), CHAT_PAGE);
        assert_eq!(
            upserts(&first)[0].id,
            format!("whatsapp:{}@s.whatsapp.net", total - 1),
            "newest first"
        );
        seen.extend(upserts(&first).iter().map(|row| row.id.clone()));
        let second = inbox.next_chat_page();
        assert_eq!(second.len(), CHAT_PAGE);
        seen.extend(upserts(&second).iter().map(|row| row.id.clone()));
        let third = inbox.next_chat_page();
        assert_eq!(third.len(), 50);
        seen.extend(upserts(&third).iter().map(|row| row.id.clone()));
        assert_eq!(seen.len(), total, "every chat is listed once");
        assert!(inbox.next_chat_page().is_empty(), "after the last page");
        assert_eq!(inbox.chat_page().len(), CHAT_PAGE, "the first page again");
    }

    #[test]
    fn clear_removes_every_chat() {
        let mut inbox = Inbox::default();
        inbox.apply_messages(vec![message("1@s.whatsapp.net", "a", "x", 1)]);
        let events = inbox.clear();
        assert_eq!(
            events,
            vec![AdapterEvent::ConversationRemoved {
                protocol: ProtocolId::WhatsApp,
                id: "whatsapp:1@s.whatsapp.net".into(),
            }]
        );
        assert!(inbox.chat_page().is_empty());
    }
}
