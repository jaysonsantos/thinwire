//! OS secret store for Telegram `api_id`, `api_hash`, and session material.
//!
//! The UI thread only touches an in-memory map. OS keychain I/O runs on a
//! tokio `spawn_blocking` worker so egui never waits on keyutils / Keychain /
//! Credential Manager. Never log or persist these values in the git repo.
//!
//! Attach is an ordered state machine (`Detached` → `Attaching` → `Ready` or
//! `MemoryOnly`). UI writes mark keys dirty so a late hydrate cannot overwrite
//! them. A flush requested before `Ready` is deferred and runs after attach.
//! Concurrent Ready flushes coalesce onto one worker so an older OS write
//! cannot clobber newer credentials.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, Mutex};

use keyring::Entry;
use thinwire_protocol::{
    TELEGRAM_SECRET_API_HASH, TELEGRAM_SECRET_API_ID, TELEGRAM_SECRET_SERVICE,
    TELEGRAM_SECRET_SESSION,
};
use tokio::runtime::Handle;

/// Set `THINWIRE_KEYRING=memory` to skip the OS keychain (CI / local headless).
pub const KEYRING_ENV: &str = "THINWIRE_KEYRING";
const KEYRING_MEMORY: &str = "memory";

/// Telegram secrets the login screens persist.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum SecretKey {
    ApiId,
    ApiHash,
    Session,
}

impl SecretKey {
    pub const ALL: [Self; 3] = [Self::ApiId, Self::ApiHash, Self::Session];

    #[must_use]
    pub const fn account(self) -> &'static str {
        match self {
            Self::ApiId => TELEGRAM_SECRET_API_ID,
            Self::ApiHash => TELEGRAM_SECRET_API_HASH,
            Self::Session => TELEGRAM_SECRET_SESSION,
        }
    }
}

impl fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ApiId => "ApiId",
            Self::ApiHash => "ApiHash",
            Self::Session => "Session",
        })
    }
}

/// Recoverable secret-store failure. Display text never includes secret values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretError {
    message: String,
}

impl SecretError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for SecretError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SecretError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttachPhase {
    Detached,
    Attaching,
    Ready,
    MemoryOnly,
}

struct Inner {
    values: HashMap<SecretKey, String>,
    dirty: HashSet<SecretKey>,
    flush_pending: bool,
    flush_in_flight: bool,
    phase: AttachPhase,
}

/// Memory-first store. OS keychain attach/flush is worker-only.
pub struct SecretStore {
    inner: Mutex<Inner>,
}

impl SecretStore {
    fn blank(phase: AttachPhase) -> Self {
        Self {
            inner: Mutex::new(Inner {
                values: HashMap::new(),
                dirty: HashSet::new(),
                flush_pending: false,
                flush_in_flight: false,
                phase,
            }),
        }
    }

    /// UI-safe constructor. Does not talk to the OS keychain.
    #[must_use]
    pub fn memory() -> Self {
        Self::blank(AttachPhase::MemoryOnly)
    }

    /// UI constructor plus a worker that attaches the OS keychain if available.
    #[must_use]
    pub fn for_ui(handle: &Handle) -> Arc<Self> {
        if memory_requested() {
            tracing::info!(
                "{KEYRING_ENV}={KEYRING_MEMORY}; Telegram secrets stay in memory this session"
            );
            return Arc::new(Self::memory());
        }
        let store = Arc::new(Self::blank(AttachPhase::Detached));
        store.spawn_os_attach(handle);
        store
    }

    /// Blocking attach used by tests. The app calls [`Self::for_ui`] instead.
    #[cfg(test)]
    #[must_use]
    pub fn open() -> Self {
        let store = Self::blank(AttachPhase::Detached);
        store.attach_os_keychain();
        store
    }

    #[must_use]
    pub fn backend_name(&self) -> &'static str {
        match self.phase() {
            AttachPhase::Ready => "os-keychain",
            AttachPhase::Detached | AttachPhase::Attaching | AttachPhase::MemoryOnly => "memory",
        }
    }

    fn phase(&self) -> AttachPhase {
        self.lock()
            .map(|inner| inner.phase)
            .unwrap_or(AttachPhase::MemoryOnly)
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Inner>, SecretError> {
        self.inner
            .lock()
            .map_err(|_| SecretError::new("memory secret store is poisoned"))
    }

    /// UI thread: update the in-memory map only. Marks the key dirty so a
    /// hydrate that finishes later cannot overwrite this write.
    pub fn set(&self, key: SecretKey, value: &str) -> Result<(), SecretError> {
        let trimmed = value.trim();
        let mut inner = self.lock()?;
        if trimmed.is_empty() {
            inner.values.remove(&key);
        } else {
            inner.values.insert(key, trimmed.to_string());
        }
        inner.dirty.insert(key);
        Ok(())
    }

    /// UI thread: read the in-memory map only.
    pub fn get(&self, key: SecretKey) -> Result<Option<String>, SecretError> {
        Ok(self.lock()?.values.get(&key).cloned())
    }

    #[cfg(test)]
    pub fn delete(&self, key: SecretKey) -> Result<(), SecretError> {
        self.set(key, "")
    }

    pub fn spawn_os_attach(self: &Arc<Self>, handle: &Handle) {
        let store = Arc::clone(self);
        handle.spawn_blocking(move || store.attach_os_keychain());
    }

    pub fn spawn_os_flush(self: &Arc<Self>, handle: &Handle) {
        match self.request_flush() {
            FlushAction::Spawn => {
                let store = Arc::clone(self);
                handle.spawn_blocking(move || {
                    if let Err(error) = store.flush_os() {
                        tracing::warn!(
                            error = %error,
                            "OS keychain flush failed; secrets stay in memory"
                        );
                    }
                });
            }
            FlushAction::Defer | FlushAction::Ignore => {}
        }
    }

    fn request_flush(&self) -> FlushAction {
        let Ok(mut inner) = self.lock() else {
            return FlushAction::Ignore;
        };
        match inner.phase {
            AttachPhase::Ready => {
                if inner.flush_in_flight {
                    inner.flush_pending = true;
                    FlushAction::Defer
                } else {
                    inner.flush_in_flight = true;
                    FlushAction::Spawn
                }
            }
            AttachPhase::Detached | AttachPhase::Attaching => {
                inner.flush_pending = true;
                FlushAction::Defer
            }
            AttachPhase::MemoryOnly => FlushAction::Ignore,
        }
    }

    /// Blocking. Worker / tests only.
    pub fn attach_os_keychain(&self) {
        {
            let Ok(mut inner) = self.lock() else {
                return;
            };
            match inner.phase {
                AttachPhase::Ready | AttachPhase::MemoryOnly => return,
                AttachPhase::Detached | AttachPhase::Attaching => {
                    inner.phase = AttachPhase::Attaching;
                }
            }
        }
        if memory_requested() {
            self.finish_memory_only();
            return;
        }
        match probe_os() {
            Ok(()) => {
                let (os_values, hydrate_error) = read_os_snapshot();
                let should_flush = self.finish_ready(os_values);
                if let Some(error) = hydrate_error {
                    tracing::warn!(
                        error = %error,
                        "OS keychain attached but hydrate failed; dirty UI writes are kept"
                    );
                } else {
                    tracing::info!("using the OS keychain for Telegram secrets");
                }
                if should_flush && let Err(error) = self.flush_os() {
                    tracing::warn!(
                        error = %error,
                        "OS keychain flush after attach failed; secrets stay in memory"
                    );
                }
            }
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "OS keychain unavailable; Telegram secrets stay in memory this session and are not written to disk"
                );
                self.finish_memory_only();
            }
        }
    }

    fn finish_memory_only(&self) {
        if let Ok(mut inner) = self.lock() {
            inner.phase = AttachPhase::MemoryOnly;
            inner.flush_pending = false;
            inner.flush_in_flight = false;
        }
    }

    /// Merge OS values under the store lock. Dirty UI keys win. Returns
    /// whether a deferred flush (or any dirty write) must hit the keychain.
    fn finish_ready(&self, os_values: HashMap<SecretKey, String>) -> bool {
        let Ok(mut inner) = self.lock() else {
            return false;
        };
        for (key, value) in os_values {
            if inner.dirty.contains(&key) {
                continue;
            }
            let trimmed = value.trim();
            if trimmed.is_empty() {
                inner.values.remove(&key);
            } else {
                inner.values.insert(key, trimmed.to_string());
            }
        }
        inner.phase = AttachPhase::Ready;
        let should_flush = inner.flush_pending || !inner.dirty.is_empty();
        inner.flush_pending = false;
        inner.dirty.clear();
        if should_flush {
            inner.flush_in_flight = true;
        }
        should_flush
    }

    fn flush_os(&self) -> Result<(), SecretError> {
        self.flush_loop(|snapshot| {
            for (key, value) in snapshot {
                match value {
                    Some(value) => os_set(*key, value)?,
                    None => os_delete(*key)?,
                }
            }
            Ok(())
        })
    }

    fn flush_loop<F>(&self, mut commit: F) -> Result<(), SecretError>
    where
        F: FnMut(&[(SecretKey, Option<String>); 3]) -> Result<(), SecretError>,
    {
        loop {
            let Some(snapshot) = self.take_flush_snapshot()? else {
                return Ok(());
            };
            if let Err(error) = commit(&snapshot) {
                if self.finish_flush_cycle()? {
                    continue;
                }
                return Err(error);
            }
            if !self.finish_flush_cycle()? {
                return Ok(());
            }
        }
    }

    fn take_flush_snapshot(&self) -> Result<Option<[(SecretKey, Option<String>); 3]>, SecretError> {
        let mut inner = self.lock()?;
        if inner.phase != AttachPhase::Ready {
            inner.flush_in_flight = false;
            inner.flush_pending = false;
            return Ok(None);
        }
        inner.flush_pending = false;
        Ok(Some(
            SecretKey::ALL.map(|key| (key, inner.values.get(&key).cloned())),
        ))
    }

    fn finish_flush_cycle(&self) -> Result<bool, SecretError> {
        let mut inner = self.lock()?;
        if inner.phase == AttachPhase::Ready && inner.flush_pending {
            inner.flush_pending = false;
            return Ok(true);
        }
        inner.flush_in_flight = false;
        Ok(false)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FlushAction {
    Spawn,
    Defer,
    Ignore,
}

fn read_os_snapshot() -> (HashMap<SecretKey, String>, Option<SecretError>) {
    let mut os_values = HashMap::new();
    for key in SecretKey::ALL {
        match os_get(key) {
            Ok(Some(value)) => {
                os_values.insert(key, value);
            }
            Ok(None) => {}
            Err(error) => return (os_values, Some(error)),
        }
    }
    (os_values, None)
}

impl fmt::Debug for SecretStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretStore")
            .field("backend", &self.backend_name())
            .field("secrets", &"<redacted>")
            .finish()
    }
}

fn memory_requested() -> bool {
    std::env::var_os(KEYRING_ENV).is_some_and(|value| value == KEYRING_MEMORY)
}

fn probe_os() -> Result<(), SecretError> {
    let entry = os_entry(SecretKey::ApiId)?;
    match entry.get_password() {
        Ok(_) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(map_keyring_error(error)),
    }
}

fn os_entry(key: SecretKey) -> Result<Entry, SecretError> {
    Entry::new(TELEGRAM_SECRET_SERVICE, key.account()).map_err(map_keyring_error)
}

fn os_set(key: SecretKey, value: &str) -> Result<(), SecretError> {
    os_entry(key)?
        .set_password(value)
        .map_err(map_keyring_error)
}

fn os_get(key: SecretKey) -> Result<Option<String>, SecretError> {
    match os_entry(key)?.get_password() {
        Ok(value) => Ok(Some(value)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(map_keyring_error(error)),
    }
}

fn os_delete(key: SecretKey) -> Result<(), SecretError> {
    match os_entry(key)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(map_keyring_error(error)),
    }
}

fn map_keyring_error(error: keyring::Error) -> SecretError {
    let kind = match error {
        keyring::Error::NoEntry => "not found",
        keyring::Error::NoStorageAccess(_) => "no storage access",
        keyring::Error::PlatformFailure(_) => "platform failure",
        keyring::Error::TooLong(_, _) => "value too long",
        keyring::Error::Invalid(_, _) => "invalid attribute",
        keyring::Error::Ambiguous(_) => "ambiguous entry",
        other => {
            let _ = other;
            "keychain error"
        }
    };
    SecretError::new(format!("OS keychain: {kind}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    impl SecretStore {
        fn force_attaching(&self) {
            let mut inner = self.lock().expect("lock");
            inner.phase = AttachPhase::Attaching;
        }

        fn complete_ready_for_test(&self, os: &[(SecretKey, &str)]) -> bool {
            let os_values = os
                .iter()
                .map(|(key, value)| (*key, (*value).to_string()))
                .collect();
            self.finish_ready(os_values)
        }

        fn finish_in_flight_flush_for_test(&self) {
            let mut inner = self.lock().expect("lock");
            inner.flush_in_flight = false;
            inner.flush_pending = false;
        }
    }

    #[test]
    fn memory_round_trip_does_not_leak_in_debug() {
        let store = SecretStore::memory();
        store.set(SecretKey::ApiId, "  12345  ").expect("set id");
        store
            .set(SecretKey::ApiHash, "super-secret-hash")
            .expect("set hash");
        store
            .set(SecretKey::Session, "super-secret-session")
            .expect("set session");
        assert_eq!(
            store.get(SecretKey::ApiId).expect("get id").as_deref(),
            Some("12345")
        );
        let debug = format!("{store:?}");
        assert!(debug.contains("memory"));
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("12345"));
        assert!(!debug.contains("super-secret-hash"));
        assert!(!debug.contains("super-secret-session"));
        store.delete(SecretKey::ApiHash).expect("delete hash");
        assert_eq!(store.get(SecretKey::ApiHash).expect("get hash"), None);
    }

    #[test]
    fn empty_value_deletes_key() {
        let store = SecretStore::memory();
        store.set(SecretKey::Session, "keep").expect("set");
        store.set(SecretKey::Session, "  ").expect("clear");
        assert_eq!(store.get(SecretKey::Session).expect("get"), None);
    }

    #[test]
    fn open_never_panics_without_a_desktop_keychain() {
        let store = SecretStore::open();
        assert!(matches!(store.backend_name(), "os-keychain" | "memory"));
        let debug = format!("{store:?}");
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn ui_set_does_not_require_os_keychain() {
        let store = SecretStore::memory();
        assert_eq!(store.backend_name(), "memory");
        store.set(SecretKey::ApiId, "11111").expect("memory set");
        assert_eq!(
            store.get(SecretKey::ApiId).expect("get").as_deref(),
            Some("11111")
        );
    }

    #[test]
    fn secret_error_display_has_no_value() {
        let err = SecretError::new("OS keychain: no storage access");
        assert!(!err.to_string().contains("api_hash"));
        assert!(!format!("{err:?}").contains("password"));
    }

    #[test]
    fn hydrate_does_not_overwrite_dirty_ui_writes() {
        let store = SecretStore::blank(AttachPhase::Attaching);
        store.set(SecretKey::ApiId, "11111").expect("ui write");
        let should_flush = store.complete_ready_for_test(&[
            (SecretKey::ApiId, "stale-os-id"),
            (SecretKey::ApiHash, "placeholder-hash"),
        ]);
        assert!(should_flush);
        assert_eq!(store.backend_name(), "os-keychain");
        assert_eq!(
            store.get(SecretKey::ApiId).expect("id").as_deref(),
            Some("11111")
        );
        assert_eq!(
            store.get(SecretKey::ApiHash).expect("hash").as_deref(),
            Some("placeholder-hash")
        );
        let debug = format!("{store:?}");
        assert!(!debug.contains("11111"));
        assert!(!debug.contains("stale-os-id"));
        assert!(!debug.contains("placeholder-hash"));
    }

    #[test]
    fn flush_during_attach_is_deferred_until_ready() {
        let store = SecretStore::blank(AttachPhase::Attaching);
        store.set(SecretKey::ApiId, "11111").expect("ui write");
        assert_eq!(store.request_flush(), FlushAction::Defer);
        assert_eq!(store.backend_name(), "memory");
        let should_flush = store.complete_ready_for_test(&[(SecretKey::ApiId, "stale-os-id")]);
        assert!(should_flush);
        assert_eq!(store.request_flush(), FlushAction::Defer);
        store.finish_in_flight_flush_for_test();
        assert_eq!(store.request_flush(), FlushAction::Spawn);
        assert_eq!(
            store.get(SecretKey::ApiId).expect("id").as_deref(),
            Some("11111")
        );
    }

    #[test]
    fn flush_while_memory_only_is_ignored() {
        let store = SecretStore::memory();
        store.set(SecretKey::ApiId, "11111").expect("ui write");
        assert_eq!(store.request_flush(), FlushAction::Ignore);
        assert_eq!(store.backend_name(), "memory");
    }

    #[test]
    fn attaching_phase_stays_memory_until_ready() {
        let store = SecretStore::blank(AttachPhase::Detached);
        store.force_attaching();
        assert_eq!(store.backend_name(), "memory");
        assert_eq!(store.phase(), AttachPhase::Attaching);
    }

    #[test]
    fn second_ready_flush_is_deferred_while_one_is_in_flight() {
        let store = SecretStore::blank(AttachPhase::Ready);
        store.set(SecretKey::ApiId, "11111").expect("set");
        assert_eq!(store.request_flush(), FlushAction::Spawn);
        assert_eq!(store.request_flush(), FlushAction::Defer);
        store.finish_in_flight_flush_for_test();
        assert_eq!(store.request_flush(), FlushAction::Spawn);
    }

    #[test]
    fn overlapping_flush_is_coalesced_and_commits_latest() {
        let store = Arc::new(SecretStore::blank(AttachPhase::Ready));
        store.set(SecretKey::ApiId, "old-id").expect("old id");
        store.set(SecretKey::ApiHash, "old-hash").expect("old hash");

        let dest = StallSink::new();
        assert_eq!(store.request_flush(), FlushAction::Spawn);

        let worker_store = Arc::clone(&store);
        let worker_dest = dest.clone();
        let worker = std::thread::spawn(move || {
            worker_store
                .flush_loop(|snapshot| worker_dest.commit(snapshot))
                .expect("flush");
        });

        dest.wait_started();
        store.set(SecretKey::ApiId, "new-id").expect("new id");
        store.set(SecretKey::ApiHash, "new-hash").expect("new hash");
        store
            .set(SecretKey::Session, "new-session")
            .expect("new session");
        assert_eq!(store.request_flush(), FlushAction::Defer);

        dest.release();
        worker.join().expect("join");

        let commits = dest.commits();
        assert!(
            commits.len() >= 2,
            "stale snapshot plus a repeat after the overlapping request"
        );
        let last = commits.last().expect("last commit");
        assert_eq!(
            snapshot_value(last, SecretKey::ApiId).as_deref(),
            Some("new-id")
        );
        assert_eq!(
            snapshot_value(last, SecretKey::ApiHash).as_deref(),
            Some("new-hash")
        );
        assert_eq!(
            snapshot_value(last, SecretKey::Session).as_deref(),
            Some("new-session")
        );
    }

    fn snapshot_value(
        snapshot: &[(SecretKey, Option<String>); 3],
        key: SecretKey,
    ) -> Option<String> {
        snapshot
            .iter()
            .find(|(item, _)| *item == key)
            .and_then(|(_, value)| value.clone())
    }

    #[derive(Clone)]
    struct StallSink {
        commits: Arc<Mutex<Vec<[(SecretKey, Option<String>); 3]>>>,
        started_rx: Arc<Mutex<Option<std::sync::mpsc::Receiver<()>>>>,
        started_tx: Arc<Mutex<Option<std::sync::mpsc::Sender<()>>>>,
        release_rx: Arc<Mutex<Option<std::sync::mpsc::Receiver<()>>>>,
        release_tx: Arc<Mutex<Option<std::sync::mpsc::Sender<()>>>>,
    }

    impl StallSink {
        fn new() -> Self {
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            Self {
                commits: Arc::new(Mutex::new(Vec::new())),
                started_rx: Arc::new(Mutex::new(Some(started_rx))),
                started_tx: Arc::new(Mutex::new(Some(started_tx))),
                release_rx: Arc::new(Mutex::new(Some(release_rx))),
                release_tx: Arc::new(Mutex::new(Some(release_tx))),
            }
        }

        fn wait_started(&self) {
            self.started_rx
                .lock()
                .expect("started rx")
                .take()
                .expect("started rx")
                .recv()
                .expect("started");
        }

        fn release(&self) {
            self.release_tx
                .lock()
                .expect("release tx")
                .take()
                .expect("release tx")
                .send(())
                .expect("release");
        }

        fn commits(&self) -> Vec<[(SecretKey, Option<String>); 3]> {
            self.commits.lock().expect("commits").clone()
        }

        fn commit(&self, snapshot: &[(SecretKey, Option<String>); 3]) -> Result<(), SecretError> {
            if let Some(tx) = self.started_tx.lock().expect("started tx").take() {
                tx.send(()).expect("signal start");
                self.release_rx
                    .lock()
                    .expect("release rx")
                    .take()
                    .expect("release rx")
                    .recv()
                    .expect("release");
            }
            self.commits.lock().expect("commits").push(snapshot.clone());
            Ok(())
        }
    }
}
