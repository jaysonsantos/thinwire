// SPDX-License-Identifier: AGPL-3.0-only
//! The chat cache on disk: `inbox.sqlite` beside the device store.
//!
//! WhatsApp sends the chat history only when a device pairs. A saved
//! session that opens again gets new messages only. The helper thus keeps
//! the chat list and the newest messages of each chat in this file and
//! loads them when the saved session opens.
//!
//! SQLCipher encrypts the file with the session key of the device store.
//! The key stays in the OS keychain. One row holds one chat as JSON. A file
//! that does not open with the key is a cache only: the helper removes it
//! and starts an empty one. No error names a path, a chat, or a message.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, params};

use super::inbox::{InboxChanges, InboxStore, SavedChat, SavedInbox};

/// File name of the chat cache in the session folder.
pub(super) const CACHE_FILE: &str = "inbox.sqlite";

/// SQLite side files of the cache.
const CACHE_SIDE_SUFFIXES: [&str; 3] = ["-wal", "-shm", "-journal"];

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS chats (jid TEXT PRIMARY KEY NOT NULL, data TEXT NOT NULL);
    CREATE TABLE IF NOT EXISTS names (jid TEXT PRIMARY KEY NOT NULL, name TEXT NOT NULL);
    CREATE TABLE IF NOT EXISTS contacts (jid TEXT PRIMARY KEY NOT NULL, name TEXT NOT NULL);
    CREATE TABLE IF NOT EXISTS phones (lid TEXT PRIMARY KEY NOT NULL, phone TEXT NOT NULL);
";

/// Write one chat row, or replace it.
const UPSERT_CHAT: &str = "INSERT INTO chats (jid, data) VALUES (?1, ?2) ON CONFLICT(jid) DO UPDATE SET data = excluded.data";

/// The chat cache of one session folder.
pub(super) struct DiskCache {
    path: PathBuf,
    key: [u8; 32],
}

impl DiskCache {
    /// The cache beside `device_store`, opened with `key`.
    pub(super) fn beside(device_store: &Path, key: [u8; 32]) -> Self {
        let path = device_store.with_file_name(CACHE_FILE);
        Self { path, key }
    }

    fn connect(&self) -> rusqlite::Result<Connection> {
        let conn = Connection::open(&self.path)?;
        let mut sql = format!(
            "PRAGMA key = {};",
            thinwire_protocol::raw_key_literal(&self.key)
        );
        let keyed = conn.execute_batch(&sql);
        sql.replace_range(.., "");
        keyed?;
        // A wrong key fails here, on the first read.
        conn.query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(()))?;
        conn.execute_batch(SCHEMA)?;
        let _ = super::path::restrict_store_file(&self.path);
        Ok(conn)
    }

    /// Open the cache. A file that does not open is removed, and an empty
    /// cache starts in its place.
    fn open(&self) -> Result<Connection, ()> {
        match self.connect() {
            Ok(conn) => Ok(conn),
            Err(_) => {
                tracing::warn!("whatsapp chat cache did not open; starting an empty one");
                remove_cache_files(&self.path);
                self.connect().map_err(|_| ())
            }
        }
    }

    /// The saved chat list. An unreadable row is skipped.
    pub(super) fn load(&self) -> SavedInbox {
        let Ok(conn) = self.open() else {
            return SavedInbox::default();
        };
        let mut saved = SavedInbox::default();
        let pair = |row: &rusqlite::Row<'_>| -> rusqlite::Result<(String, String)> {
            Ok((row.get(0)?, row.get(1)?))
        };
        if let Ok(mut rows) = conn.prepare("SELECT jid, data FROM chats")
            && let Ok(read) = rows.query_map([], pair)
        {
            for (jid, data) in read.flatten() {
                match serde_json::from_str::<SavedChat>(&data) {
                    Ok(chat) => saved.chats.push((jid, chat)),
                    Err(_) => tracing::warn!("whatsapp chat cache: skipped a row"),
                }
            }
        }
        for (sql, rows) in [
            ("SELECT jid, name FROM names", &mut saved.names),
            ("SELECT jid, name FROM contacts", &mut saved.contacts),
            ("SELECT lid, phone FROM phones", &mut saved.phones),
        ] {
            if let Ok(mut statement) = conn.prepare(sql)
                && let Ok(read) = statement.query_map([], pair)
            {
                rows.extend(read.flatten());
            }
        }
        saved
    }

    /// Remove the cache and its side files.
    pub(super) fn remove(&self) {
        remove_cache_files(&self.path);
    }
}

impl InboxStore for DiskCache {
    fn save(&self, changes: &InboxChanges) -> Result<(), ()> {
        let mut conn = self.open()?;
        let tx = conn.transaction().map_err(|_| ())?;
        for (jid, _, chat) in &changes.chats {
            let data = serde_json::to_string(chat).map_err(|_| ())?;
            tx.execute(UPSERT_CHAT, params![jid, data])
                .map_err(|_| ())?;
        }
        for jid in &changes.removed {
            tx.execute("DELETE FROM chats WHERE jid = ?1", params![jid])
                .map_err(|_| ())?;
        }
        for (clear, insert, rows) in [
            (
                "DELETE FROM names",
                "INSERT INTO names (jid, name) VALUES (?1, ?2)",
                &changes.names,
            ),
            (
                "DELETE FROM contacts",
                "INSERT INTO contacts (jid, name) VALUES (?1, ?2)",
                &changes.contacts,
            ),
            (
                "DELETE FROM phones",
                "INSERT INTO phones (lid, phone) VALUES (?1, ?2)",
                &changes.phones,
            ),
        ] {
            let Some((_, rows)) = rows else {
                continue;
            };
            tx.execute(clear, []).map_err(|_| ())?;
            for (key, value) in rows {
                tx.execute(insert, params![key, value]).map_err(|_| ())?;
            }
        }
        tx.commit().map_err(|_| ())
    }
}

/// The cache file beside `device_store`, for a delete of the whole store.
pub(super) fn cache_path(device_store: &Path) -> PathBuf {
    device_store.with_file_name(CACHE_FILE)
}

/// Remove the cache file and its side files. A missing file is fine.
pub(super) fn remove_cache_files(path: &Path) {
    let _ = std::fs::remove_file(path);
    for suffix in CACHE_SIDE_SUFFIXES {
        let mut side = path.as_os_str().to_owned();
        side.push(suffix);
        let _ = std::fs::remove_file(PathBuf::from(side));
    }
}

/// The cache holds one row per chat: a test can check that a row exists.
#[cfg(test)]
fn has_chat(cache: &DiskCache, jid: &str) -> bool {
    use rusqlite::OptionalExtension as _;
    cache
        .open()
        .ok()
        .and_then(|conn| {
            conn.query_row("SELECT 1 FROM chats WHERE jid = ?1", params![jid], |_| {
                Ok(())
            })
            .optional()
            .ok()
            .flatten()
        })
        .is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::whatsapp::inbox::tests::message;
    use crate::whatsapp::inbox::{Inbox, SavedHashes};

    fn folder(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("thinwire-wa-cache-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("folder");
        dir
    }

    /// A save and a load give the same chat list back. The file is
    /// encrypted: another key does not read it, and a new empty cache starts.
    #[test]
    fn the_cache_keeps_the_chats_and_needs_the_key() {
        let dir = folder("round-trip");
        let store = dir.join("device.sqlite");
        let cache = DiskCache::beside(&store, [7; 32]);
        let mut inbox = Inbox::default();
        let chat = "111@s.whatsapp.net";
        let _ = inbox.apply_messages(vec![
            message(chat, "m1", "secret-cache-body", 5),
            message(chat, "m2", "two", 6),
        ]);
        let mut hashes = SavedHashes::default();
        let changes = inbox.changes(&hashes);
        assert_eq!(changes.chats.len(), 1);
        cache.save(&changes).expect("save");
        hashes.apply(&changes);
        assert!(inbox.changes(&hashes).is_empty(), "nothing new to write");
        assert!(has_chat(&cache, chat));

        let raw = std::fs::read(dir.join(CACHE_FILE)).expect("file");
        assert!(
            !raw.windows(17).any(|part| part == b"secret-cache-body"),
            "the file is encrypted"
        );

        let loaded = cache.load();
        let mut restored = Inbox::default();
        let hashes = restored.restore(loaded);
        assert!(restored.changes(&hashes).is_empty(), "restored = saved");
        let page = restored.chat_page();
        assert_eq!(page.len(), 1, "the chat row comes back: {page:?}");

        let other = DiskCache::beside(&store, [8; 32]);
        assert!(other.load().chats.is_empty(), "another key reads nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The address-book names and the LID pairs go to the file and come
    /// back.
    #[test]
    fn the_cache_keeps_the_contacts_and_the_lid_pairs() {
        let dir = folder("people");
        let cache = DiskCache::beside(&dir.join("device.sqlite"), [5; 32]);
        let mut inbox = Inbox::default();
        let lid = "223853863252019@lid";
        let phone = "5511999990000@s.whatsapp.net";
        let _ = inbox.apply_contacts(vec![crate::whatsapp::inbox::WaContact {
            jid: phone.into(),
            name: Some("Ana Maria".into()),
            lid: Some(lid.into()),
            phone: None,
        }]);
        let changes = inbox.changes(&SavedHashes::default());
        cache.save(&changes).expect("save");
        let loaded = cache.load();
        assert_eq!(loaded.phones, vec![(lid.to_string(), phone.to_string())]);
        assert_eq!(loaded.contacts.len(), 2, "the LID and the phone JID");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A removed chat leaves the cache.
    #[test]
    fn a_removed_chat_leaves_the_cache() {
        let dir = folder("removed");
        let cache = DiskCache::beside(&dir.join("device.sqlite"), [3; 32]);
        let mut inbox = Inbox::default();
        let chat = "222@s.whatsapp.net";
        let _ = inbox.apply_messages(vec![message(chat, "m1", "hi", 5)]);
        let mut hashes = SavedHashes::default();
        let changes = inbox.changes(&hashes);
        cache.save(&changes).expect("save");
        hashes.apply(&changes);
        let _ = inbox.clear();
        let changes = inbox.changes(&hashes);
        assert_eq!(changes.removed, vec![chat.to_string()]);
        cache.save(&changes).expect("save");
        assert!(!has_chat(&cache, chat));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
