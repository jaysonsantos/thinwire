//! Chats muted in thinwire (#153), for every protocol.
//!
//! A protocol mute comes from the adapter (`Conversation::muted`). Slack,
//! Discord, and Signal cannot read the user's mute, so thinwire keeps its
//! own set here. The notification rules treat both the same, and a protocol
//! mute wins: a thinwire unmute never unmutes it.
//!
//! The set is stored in `<data dir>/thinwire/muted_chats`, one
//! `<protocol> <conversation_id>` per line, with mode 0600 on Unix. A chat
//! id can hold personal data (a WhatsApp id holds a phone number), so the
//! file is not in the config dir, and no log line names an id.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use thinwire_protocol::ProtocolId;

use crate::settings::PersistJob;

/// The file name in the thinwire data dir.
const FILE_NAME: &str = "muted_chats";

/// The first line of the file. A line that starts with `#` is skipped.
const HEADER: &str = "# Chats muted in thinwire. One '<protocol> <conversation_id>' per line.";

/// Muted chat ids of each protocol.
type Chats = HashMap<ProtocolId, HashSet<String>>;

/// The mute of one chat, as the view shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatMute {
    /// The chat can notify.
    None,
    /// Muted in thinwire. Unmute in thinwire.
    Here,
    /// Muted in the protocol, for example on the phone. It wins over a
    /// thinwire mute, and only the protocol can unmute it.
    Protocol,
}

impl ChatMute {
    /// The chat does not notify and does not count in the unread total.
    #[must_use]
    pub const fn is_muted(self) -> bool {
        !matches!(self, Self::None)
    }

    /// Protocol mute first, then the thinwire mute.
    #[must_use]
    pub const fn of(protocol_muted: bool, muted_here: bool) -> Self {
        if protocol_muted {
            Self::Protocol
        } else if muted_here {
            Self::Here
        } else {
            Self::None
        }
    }
}

/// The chats muted in thinwire, and the queued write of the file.
#[derive(Debug, Clone)]
pub struct ChatMutes {
    chats: Chats,
    /// `None`: never read or write a file (the demo and tests).
    path: Option<PathBuf>,
    persist_pending: bool,
    persist_epoch: u64,
    latest_persist: Arc<AtomicU64>,
    persist_lock: Arc<Mutex<()>>,
}

impl ChatMutes {
    /// Load from the thinwire data dir. A missing or unreadable file means
    /// no mute.
    #[must_use]
    pub fn load() -> Self {
        match dirs::data_dir() {
            Some(root) => Self::load_from(root.join("thinwire").join(FILE_NAME)),
            None => Self::in_memory(),
        }
    }

    /// Load from `path`. A bad line is skipped; the next write drops it. A
    /// missing file means no mute. Any other read error keeps the mutes in
    /// memory for this run and never writes the file, so a file that could
    /// not be read does not lose its list (#201 qa).
    #[must_use]
    pub fn load_from(path: PathBuf) -> Self {
        match fs::read(&path) {
            Ok(bytes) => Self {
                path: Some(path),
                ..Self::with_chats(parse(&bytes))
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self {
                path: Some(path),
                ..Self::in_memory()
            },
            Err(error) => {
                // The kind only: the path names the user.
                tracing::warn!(
                    kind = ?error.kind(),
                    "muted chats could not be read; new mutes stay in memory for this run"
                );
                Self::in_memory()
            }
        }
    }

    /// No file: a mute stays in memory.
    #[must_use]
    pub fn in_memory() -> Self {
        Self::with_chats(Chats::new())
    }

    fn with_chats(chats: Chats) -> Self {
        Self {
            chats,
            path: None,
            persist_pending: false,
            persist_epoch: 0,
            latest_persist: Arc::new(AtomicU64::new(0)),
            persist_lock: Arc::new(Mutex::new(())),
        }
    }

    /// The chat is muted in thinwire.
    #[must_use]
    pub fn contains(&self, protocol: ProtocolId, conversation_id: &str) -> bool {
        self.chats
            .get(&protocol)
            .is_some_and(|ids| ids.contains(conversation_id))
    }

    /// Mute or unmute a chat. Returns `true` when the set changed. A disk
    /// write is queued.
    pub fn set(&mut self, protocol: ProtocolId, conversation_id: &str, muted: bool) -> bool {
        let changed = if muted {
            self.chats
                .entry(protocol)
                .or_default()
                .insert(conversation_id.to_owned())
        } else {
            self.chats
                .get_mut(&protocol)
                .is_some_and(|ids| ids.remove(conversation_id))
        };
        self.persist_pending |= changed;
        changed
    }

    /// Drop every mute of `protocol`: its account ended for good (#153
    /// review). A disk write is queued when one was set.
    pub fn forget(&mut self, protocol: ProtocolId) {
        let changed = self
            .chats
            .remove(&protocol)
            .is_some_and(|ids| !ids.is_empty());
        self.persist_pending |= changed;
    }

    /// Take the latest queued write. The caller runs it off the UI thread.
    #[must_use]
    pub fn take_persist_job(&mut self) -> Option<PersistJob> {
        if !std::mem::take(&mut self.persist_pending) {
            return None;
        }
        let path = self.path.clone()?;
        self.persist_epoch = self.persist_epoch.saturating_add(1);
        self.latest_persist
            .store(self.persist_epoch, Ordering::Release);
        Some(PersistJob::private(
            path,
            render(&self.chats),
            self.persist_epoch,
            Arc::clone(&self.latest_persist),
            Arc::clone(&self.persist_lock),
        ))
    }
}

/// The file key of a protocol. Never changes: the file keeps it.
const fn key(protocol: ProtocolId) -> &'static str {
    match protocol {
        ProtocolId::Telegram => "telegram",
        ProtocolId::WhatsApp => "whatsapp",
        ProtocolId::Discord => "discord",
        ProtocolId::Slack => "slack",
        ProtocolId::Signal => "signal",
    }
}

fn protocol_of(key_text: &str) -> Option<ProtocolId> {
    ProtocolId::ALL
        .into_iter()
        .find(|protocol| key(*protocol) == key_text)
}

/// The mutes in the file. A line that is empty, a comment, not UTF-8, or
/// not `<known protocol> <id>` is skipped. One bad byte skips its line
/// only. Only a count goes to the log.
fn parse(contents: &[u8]) -> Chats {
    let mut skipped = 0_usize;
    let mut chats = Chats::new();
    let lines = contents
        .split(|byte| *byte == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        .filter(|line| !line.trim_ascii().is_empty() && !line.starts_with(b"#"))
        .filter_map(|line| {
            let parsed = std::str::from_utf8(line)
                .ok()
                .and_then(|line| line.split_once(' '))
                .and_then(|(protocol, id)| Some((protocol_of(protocol)?, id)))
                .filter(|(_, id)| !id.is_empty())
                .map(|(protocol, id)| (protocol, id.to_owned()));
            skipped += usize::from(parsed.is_none());
            parsed
        });
    for (protocol, id) in lines {
        chats.entry(protocol).or_default().insert(id);
    }
    if skipped > 0 {
        tracing::warn!(skipped, "muted chats: bad lines were skipped");
    }
    chats
}

/// The file text, sorted. An id with a line break stays muted in memory
/// only: it cannot be one line.
fn render(chats: &Chats) -> String {
    let mut text = format!("{HEADER}\n");
    for protocol in ProtocolId::ALL {
        let Some(ids) = chats.get(&protocol) else {
            continue;
        };
        let sorted: BTreeSet<&String> = ids.iter().collect();
        for id in sorted {
            if id.contains(['\n', '\r']) {
                continue;
            }
            text.push_str(key(protocol));
            text.push(' ');
            text.push_str(id);
            text.push('\n');
        }
    }
    text
}

/// Write `contents` to a new temp file with mode 0600, then rename it over
/// `path`. A reader sees the old file or the new one, never a part.
pub(crate) fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    create_private_dir(parent)?;
    let name = path
        .file_name()
        .map_or_else(|| FILE_NAME.into(), |name| name.to_string_lossy());
    let temp = parent.join(format!(".{name}.{}.tmp", std::process::id()));
    let _ = fs::remove_file(&temp);
    let written = open_private(&temp).and_then(|mut file| {
        file.write_all(contents.as_bytes())?;
        file.sync_all()
    });
    let result = written.and_then(|()| fs::rename(&temp, path));
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)
}

#[cfg(unix)]
fn open_private(path: &Path) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn open_private(path: &Path) -> std::io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join("thinwire-mutes-tests")
            .join(format!("{}-{name}", std::process::id()))
            .join(FILE_NAME)
    }

    #[test]
    fn a_protocol_mute_wins_over_a_thinwire_mute() {
        assert_eq!(ChatMute::of(false, false), ChatMute::None);
        assert_eq!(ChatMute::of(false, true), ChatMute::Here);
        assert_eq!(ChatMute::of(true, false), ChatMute::Protocol);
        assert_eq!(ChatMute::of(true, true), ChatMute::Protocol);
        assert!(!ChatMute::None.is_muted());
        assert!(ChatMute::Here.is_muted());
        assert!(ChatMute::Protocol.is_muted());
    }

    #[test]
    fn mutes_round_trip_through_a_private_file() {
        let path = temp_file("round-trip");
        let _ = fs::remove_file(&path);
        let mut mutes = ChatMutes::load_from(path.clone());
        let ids = [
            (ProtocolId::Telegram, "telegram:-100123"),
            (ProtocolId::WhatsApp, "whatsapp:4915550100@s.whatsapp.net"),
            (ProtocolId::Signal, "signal:group:a/b+c=="),
            (ProtocolId::Slack, "slack:T1:C1"),
            (ProtocolId::Discord, "discord:1 2"),
        ];
        for (protocol, id) in ids {
            assert!(mutes.set(protocol, id, true));
        }
        assert!(
            !mutes.set(ProtocolId::Slack, "slack:T1:C1", true),
            "no change"
        );
        mutes.take_persist_job().expect("a write").run();
        assert!(mutes.take_persist_job().is_none(), "one write per change");

        let loaded = ChatMutes::load_from(path.clone());
        for (protocol, id) in ids {
            assert!(loaded.contains(protocol, id), "{id}");
        }
        assert!(!loaded.contains(ProtocolId::Telegram, "slack:T1:C1"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).expect("file").permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let parent = path.parent().expect("dir");
        let names: Vec<_> = fs::read_dir(parent)
            .expect("dir")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert_eq!(
            names,
            vec![std::ffi::OsString::from(FILE_NAME)],
            "no temp file left"
        );

        let mut loaded = loaded;
        assert!(loaded.set(
            ProtocolId::WhatsApp,
            "whatsapp:4915550100@s.whatsapp.net",
            false
        ));
        loaded.take_persist_job().expect("a write").run();
        let again = ChatMutes::load_from(path);
        assert!(!again.contains(ProtocolId::WhatsApp, "whatsapp:4915550100@s.whatsapp.net"));
        assert!(again.contains(ProtocolId::Telegram, "telegram:-100123"));
    }

    #[test]
    fn a_bad_line_is_skipped() {
        let chats = parse(
            b"# comment\n\ntelegram telegram:1\nnot-a-protocol x\ntelegram\nslack \nslack slack:\xff\nslack slack:C1\r\n",
        );
        assert_eq!(
            render(&chats),
            format!("{HEADER}\ntelegram telegram:1\nslack slack:C1\n")
        );
    }

    /// #201 qa: one bad byte skips its line only, and the next write keeps
    /// the other mutes.
    #[test]
    fn a_bad_byte_skips_one_line_and_the_next_write_keeps_the_rest() {
        let path = temp_file("bad-byte");
        fs::create_dir_all(path.parent().expect("dir")).expect("dir");
        fs::write(
            &path,
            b"telegram telegram:1\nslack slack:\xfe\xff\nslack slack:C1\n",
        )
        .expect("file");
        let mut mutes = ChatMutes::load_from(path.clone());
        assert!(mutes.contains(ProtocolId::Telegram, "telegram:1"));
        assert!(mutes.contains(ProtocolId::Slack, "slack:C1"));
        assert!(mutes.set(ProtocolId::Discord, "discord:9", true));
        mutes.take_persist_job().expect("a write").run();
        let again = ChatMutes::load_from(path);
        assert!(again.contains(ProtocolId::Telegram, "telegram:1"));
        assert!(again.contains(ProtocolId::Slack, "slack:C1"));
        assert!(again.contains(ProtocolId::Discord, "discord:9"));
    }

    /// #201 qa: a file that cannot be read keeps its list. New mutes work
    /// in memory, and no write replaces the file.
    #[test]
    fn an_unreadable_file_is_never_replaced() {
        // A directory at the path: the read fails, and not with NotFound.
        let path = temp_file("unreadable");
        fs::create_dir_all(&path).expect("dir");
        let mut mutes = ChatMutes::load_from(path.clone());
        assert!(mutes.set(ProtocolId::Telegram, "telegram:1", true));
        assert!(mutes.contains(ProtocolId::Telegram, "telegram:1"));
        assert!(mutes.take_persist_job().is_none(), "no write");
        assert!(path.is_dir(), "the path is not replaced");
    }

    /// #153 review: an ended account drops its protocol's mutes only.
    #[test]
    fn forget_drops_one_protocol_and_queues_a_write() {
        let mut mutes = ChatMutes::load_from(temp_file("forget"));
        mutes.set(ProtocolId::Telegram, "telegram:1", true);
        mutes.set(ProtocolId::Slack, "slack:C1", true);
        let _ = mutes.take_persist_job();
        mutes.forget(ProtocolId::Telegram);
        assert!(!mutes.contains(ProtocolId::Telegram, "telegram:1"));
        assert!(mutes.contains(ProtocolId::Slack, "slack:C1"));
        assert!(mutes.take_persist_job().is_some(), "the file changes");
        mutes.forget(ProtocolId::Telegram);
        assert!(mutes.take_persist_job().is_none(), "nothing to drop");
    }

    #[test]
    fn a_missing_file_means_no_mute_and_memory_mutes_never_write() {
        let missing = ChatMutes::load_from(temp_file("missing"));
        assert!(!missing.contains(ProtocolId::Telegram, "telegram:1"));
        let mut memory = ChatMutes::in_memory();
        assert!(memory.set(ProtocolId::Telegram, "telegram:1", true));
        assert!(memory.contains(ProtocolId::Telegram, "telegram:1"));
        assert!(memory.take_persist_job().is_none());
    }

    #[test]
    fn an_id_with_a_line_break_is_not_written() {
        let mut chats = Chats::new();
        chats
            .entry(ProtocolId::Slack)
            .or_default()
            .extend(["slack:bad\nline".to_owned(), "slack:C1".to_owned()]);
        assert_eq!(render(&chats), format!("{HEADER}\nslack slack:C1\n"));
    }
}
