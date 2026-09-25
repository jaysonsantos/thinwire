//! OS secret store for Telegram `api_id`, `api_hash`, and session material.
//!
//! The UI thread only touches an in-memory map. OS keychain I/O runs on a
//! tokio `spawn_blocking` worker so a frontend never waits on keyutils / Keychain /
//! Credential Manager. Never log or persist these values in the git repo.
//!
//! Attach is an ordered state machine (`Detached` → `Attaching` → `Ready` or
//! `MemoryOnly`). A failed key read stays `Attaching`: the partial snapshot is
//! not applied and the phase does not become `Ready`, so a missing database
//! key is not assumed. UI writes mark keys dirty so a late hydrate cannot
//! overwrite them. A flush requested before `Ready` is deferred and runs after
//! attach.
//! Concurrent Ready flushes coalesce onto one worker so an older OS write
//! cannot clobber newer credentials.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, Mutex, OnceLock};

use keyring_core::Entry;
use thinwire_protocol::{
    DISCORD_SECRET_BOT_TOKEN, DISCORD_SECRET_SERVICE, DiscordSecretVault, SLACK_SECRET_SERVICE,
    SlackSecretKey, SlackSecretVault, TDLIB_FOLDER, TDLIB_KEYUTILS_FOLDER, TELEGRAM_SECRET_SERVICE,
    TelegramSecretKey, TelegramSecretVault,
};
use tokio::runtime::Handle;

/// Telegram secrets the login screens persist. Re-export keeps call sites short.
pub type SecretKey = TelegramSecretKey;

/// Set `THINWIRE_KEYRING=memory` to skip the OS keychain (CI / local headless).
pub const KEYRING_ENV: &str = "THINWIRE_KEYRING";
const KEYRING_MEMORY: &str = "memory";

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

/// OS store in use after attach. Linux tries them in [`LINUX_BACKENDS`] order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsBackend {
    /// D-Bus Secret Service: KDE Wallet or GNOME Keyring. Kept across restarts.
    SecretService,
    /// Kernel keyutils. The keyring is lost when the computer restarts.
    KernelKeyring,
    /// macOS Keychain or Windows Credential Manager.
    Native,
}

impl OsBackend {
    #[must_use]
    pub const fn survives_restart(self) -> bool {
        !matches!(self, Self::KernelKeyring)
    }
}

/// Secret Service first, keyutils second. Memory is the last fallback.
#[cfg(target_os = "linux")]
const LINUX_BACKENDS: [OsBackend; 2] = [OsBackend::SecretService, OsBackend::KernelKeyring];

/// Set once, by the first successful store install.
static OS_BACKEND: OnceLock<OsBackend> = OnceLock::new();

/// How long a sign-in lasts with the current store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Persistence {
    /// Attach has not finished.
    Loading,
    /// Kept across restarts.
    Saved,
    /// Kept until the computer restarts (kernel keyutils).
    UntilRestart,
    /// Kept for this app session only (memory).
    ThisSession,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttachPhase {
    Detached,
    Attaching,
    Ready,
    MemoryOnly,
    /// The OS store opened, but reading an entry failed. The values are not
    /// known, so a missing key must not look like a lost key. Attach stays
    /// unsettled until Try again reads every entry.
    ReadFailed,
}

type DiscordHydrateHook = Box<dyn FnOnce() + Send>;
type SlackHydrateHook = Box<dyn FnOnce() + Send>;

enum SlackHydrate {
    Waiting { hook: Option<SlackHydrateHook> },
    Settled { notify: bool },
}

enum DiscordHydrate {
    /// OS attach has not finished. The hook runs once a token is stored.
    Waiting { hook: Option<DiscordHydrateHook> },
    /// Attach finished, or this store never talks to the OS keychain.
    /// `notify` is set only when attach stored a bot token.
    Settled { notify: bool },
}

struct Inner {
    values: HashMap<SecretKey, String>,
    dirty: HashSet<SecretKey>,
    discord_bot_token: Option<String>,
    discord_token_dirty: bool,
    slack: HashMap<SlackSecretKey, String>,
    slack_dirty: HashSet<SlackSecretKey>,
    flush_pending: bool,
    flush_in_flight: bool,
    phase: AttachPhase,
    os_backend: Option<OsBackend>,
    discord_hydrate: DiscordHydrate,
    slack_hydrate: SlackHydrate,
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
                discord_bot_token: None,
                discord_token_dirty: false,
                slack: HashMap::new(),
                slack_dirty: HashSet::new(),
                flush_pending: false,
                flush_in_flight: false,
                phase,
                os_backend: None,
                discord_hydrate: if phase == AttachPhase::MemoryOnly {
                    DiscordHydrate::Settled { notify: false }
                } else {
                    DiscordHydrate::Waiting { hook: None }
                },
                slack_hydrate: if phase == AttachPhase::MemoryOnly {
                    SlackHydrate::Settled { notify: false }
                } else {
                    SlackHydrate::Waiting { hook: None }
                },
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
            AttachPhase::Detached
            | AttachPhase::Attaching
            | AttachPhase::MemoryOnly
            | AttachPhase::ReadFailed => "memory",
        }
    }

    /// UI thread: true once OS attach finished or the store is memory-only.
    /// Values read after this point include what the keychain held at launch.
    #[must_use]
    pub fn attach_settled(&self) -> bool {
        matches!(self.phase(), AttachPhase::Ready | AttachPhase::MemoryOnly)
    }

    /// UI thread: the keychain opened, but a read failed. The UI offers Try
    /// again and does not start a sign-in (no endless spinner).
    #[must_use]
    pub fn read_failed(&self) -> bool {
        self.phase() == AttachPhase::ReadFailed
    }

    /// UI thread: how long a sign-in lasts with the store in use.
    #[must_use]
    pub fn persistence(&self) -> Persistence {
        let Ok(inner) = self.lock() else {
            return Persistence::ThisSession;
        };
        match (inner.phase, inner.os_backend) {
            (AttachPhase::Detached | AttachPhase::Attaching | AttachPhase::ReadFailed, _) => {
                Persistence::Loading
            }
            (AttachPhase::MemoryOnly, _) => Persistence::ThisSession,
            (AttachPhase::Ready, Some(backend)) if !backend.survives_restart() => {
                Persistence::UntilRestart
            }
            (AttachPhase::Ready, _) => Persistence::Saved,
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

    /// UI thread: memory only. OS keychain I/O stays on the flush/attach worker.
    pub fn set_discord_bot_token(&self, value: &str) -> Result<(), SecretError> {
        let trimmed = value.trim();
        let mut inner = self.lock()?;
        if trimmed.is_empty() {
            inner.discord_bot_token = None;
        } else {
            inner.discord_bot_token = Some(trimmed.to_string());
        }
        inner.discord_token_dirty = true;
        Ok(())
    }

    /// UI thread: read the in-memory bot token only.
    pub fn discord_bot_token(&self) -> Result<Option<String>, SecretError> {
        Ok(self.lock()?.discord_bot_token.clone())
    }

    /// UI thread: read the in-memory map only.
    pub fn get(&self, key: SecretKey) -> Result<Option<String>, SecretError> {
        Ok(self.lock()?.values.get(&key).cloned())
    }

    #[cfg(test)]
    pub fn delete(&self, key: SecretKey) -> Result<(), SecretError> {
        self.set(key, "")
    }

    /// Run `hook` once OS attach has stored a Discord bot token.
    ///
    /// If attach already finished with a token, `hook` runs on the caller.
    /// The UI thread only enqueues work; it does not read the OS keychain.
    /// A memory-only store never fires, because nothing is hydrated from the OS.
    pub fn on_discord_token_hydrated(&self, hook: impl FnOnce() + Send + 'static) {
        let mut hook = Some(hook);
        let fire_now = {
            let Ok(mut inner) = self.lock() else {
                return;
            };
            match &mut inner.discord_hydrate {
                DiscordHydrate::Waiting { hook: slot } => {
                    if let Some(hook) = hook.take() {
                        *slot = Some(Box::new(hook));
                    }
                    false
                }
                DiscordHydrate::Settled { notify } => *notify && inner.discord_bot_token.is_some(),
            }
        };
        if fire_now && let Some(hook) = hook.take() {
            hook();
        }
    }

    /// Try again after a failed read. The phase leaves `ReadFailed` before
    /// this returns, so a new keychain watch does not stop at the old failure.
    pub fn spawn_os_retry(self: &Arc<Self>, handle: &Handle) {
        self.leave_read_failed();
        self.spawn_os_attach(handle);
    }

    /// `ReadFailed` → `Attaching`, under the lock, before any worker runs.
    fn leave_read_failed(&self) {
        if let Ok(mut inner) = self.lock()
            && inner.phase == AttachPhase::ReadFailed
        {
            inner.phase = AttachPhase::Attaching;
        }
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
            AttachPhase::Detached | AttachPhase::Attaching | AttachPhase::ReadFailed => {
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
                AttachPhase::Detached | AttachPhase::Attaching | AttachPhase::ReadFailed => {
                    inner.phase = AttachPhase::Attaching;
                }
            }
        }
        if memory_requested() {
            self.finish_memory_only();
            self.signal_discord_hydrated();
            self.signal_slack_hydrated();
            return;
        }
        let probed = match probe_os() {
            Ok(backend) => Ok(backend),
            Err(AttachError::Absent(error)) => Err(error),
            Err(AttachError::ReadFailed(error)) => {
                // A store that is present but failing: its keys are unknown, not
                // missing. Try again reads it; nothing falls back (Codex 4091651519).
                tracing::warn!(
                    error = %error,
                    "OS keychain is present but could not be read; sign-in data stays unloaded until Try again"
                );
                self.finish_read_failed();
                return;
            }
        };
        match probed {
            Ok(backend) => {
                self.store_probed_secrets(
                    backend,
                    read_os_snapshot(),
                    read_discord_os_token(),
                    read_slack_os(),
                );
            }
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "OS keychain unavailable; Telegram secrets stay in memory this session and are not written to disk"
                );
                self.finish_memory_only();
                self.signal_discord_hydrated();
                self.signal_slack_hydrated();
            }
        }
    }

    /// Apply one OS probe. A Slack read error leaves attach unsettled, the
    /// same as a Telegram hydrate error: no `Ready`, and the Slack reconnect
    /// hook stays waiting for Try again.
    fn store_probed_secrets(
        &self,
        backend: OsBackend,
        telegram: Result<HashMap<SecretKey, String>, SecretError>,
        discord_token: Result<Option<String>, SecretError>,
        slack_install: Result<Vec<(SlackSecretKey, String)>, SecretError>,
    ) {
        if let Err(error) = &slack_install {
            tracing::warn!(
                error = %error,
                "slack install hydrate failed; attach stays unsettled"
            );
            self.finish_read_failed();
            return;
        }
        let should_flush = match telegram {
            Ok(os_values) => self.settle_after_probe(backend, os_values, None),
            Err(error) => self.settle_after_probe(backend, HashMap::new(), Some(error)),
        };
        self.store_hydrated_discord_token(discord_token);
        self.store_hydrated_slack(slack_install);
        if self.phase() == AttachPhase::Ready {
            tracing::info!(?backend, "using the OS keychain for Telegram secrets");
        }
        if should_flush && let Err(error) = self.flush_os() {
            tracing::warn!(
                error = %error,
                "OS keychain flush after attach failed; secrets stay in memory"
            );
        }
        self.signal_discord_hydrated();
        self.signal_slack_hydrated();
    }

    /// Unsettled after a failed read: keys unknown, the UI offers Try again.
    fn finish_read_failed(&self) {
        if let Ok(mut inner) = self.lock() {
            inner.phase = AttachPhase::ReadFailed;
        }
    }

    fn finish_memory_only(&self) {
        if let Ok(mut inner) = self.lock() {
            inner.phase = AttachPhase::MemoryOnly;
            inner.flush_pending = false;
            inner.flush_in_flight = false;
        }
    }

    /// Apply one probe result.
    ///
    /// `Ok(None)` entries are simply absent from `os_values`. That clean map
    /// may settle `Ready` with no database key, which is the keyless recovery
    /// signal. A hydrate error keeps `Attaching` and drops `os_values`, even
    /// when the map already holds a session. A partial snapshot must not
    /// become `Ready` with `DbEncryption` missing.
    fn settle_after_probe(
        &self,
        backend: OsBackend,
        os_values: HashMap<SecretKey, String>,
        hydrate_error: Option<SecretError>,
    ) -> bool {
        if let Some(error) = hydrate_error {
            tracing::warn!(
                error = %error,
                "OS keychain hydrate failed; attach stays unsettled so a missing database key is not assumed"
            );
            // Unsettled, but not "still loading": the UI shows Try again.
            self.finish_read_failed();
            return false;
        }
        self.finish_ready(os_values, Some(backend))
    }

    /// Merge OS values under the store lock. Dirty UI keys win. Returns
    /// whether a deferred flush (or any dirty write) must hit the keychain.
    /// Ready and the backend change under one lock, so a reader that sees
    /// `Ready` also sees the backend (qa M2).
    pub(crate) fn finish_ready(
        &self,
        os_values: HashMap<SecretKey, String>,
        backend: Option<OsBackend>,
    ) -> bool {
        let Ok(mut inner) = self.lock() else {
            return false;
        };
        if backend.is_some() {
            inner.os_backend = backend;
        }
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
        let should_flush = inner.flush_pending
            || !inner.dirty.is_empty()
            || inner.discord_token_dirty
            || !inner.slack_dirty.is_empty();
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
        })?;
        self.flush_discord_token()?;
        self.flush_slack()
    }

    fn flush_discord_token(&self) -> Result<(), SecretError> {
        let snapshot = {
            let mut inner = self.lock()?;
            if inner.phase != AttachPhase::Ready || !inner.discord_token_dirty {
                return Ok(());
            }
            inner.discord_token_dirty = false;
            inner.discord_bot_token.clone()
        };
        let write = match snapshot {
            Some(value) => discord_os_set(&value),
            None => discord_os_delete(),
        };
        if let Err(error) = write {
            if let Ok(mut inner) = self.lock() {
                inner.discord_token_dirty = true;
            }
            return Err(error);
        }
        Ok(())
    }

    /// Tell a waiting Discord reconnect that attach has finished.
    ///
    /// The hook runs only when a token is now in memory. Callers enqueue a
    /// connect; they do not construct the HTTP client on this thread.
    fn signal_discord_hydrated(&self) {
        let (hook, token_present) = {
            let Ok(mut inner) = self.lock() else {
                return;
            };
            let DiscordHydrate::Waiting { hook } = &mut inner.discord_hydrate else {
                return;
            };
            let hook = hook.take();
            let token_present = inner.discord_bot_token.is_some();
            inner.discord_hydrate = DiscordHydrate::Settled {
                notify: token_present,
            };
            (hook, token_present)
        };
        if token_present && let Some(hook) = hook {
            hook();
        }
    }

    fn store_hydrated_discord_token(&self, value: Result<Option<String>, SecretError>) {
        let Ok(mut inner) = self.lock() else {
            return;
        };
        if inner.discord_token_dirty {
            return;
        }
        match value {
            Ok(Some(raw)) => {
                let trimmed = raw.trim();
                inner.discord_bot_token = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                };
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(error = %error, "discord bot token hydrate failed");
            }
        }
    }

    /// Run `hook` once OS attach has stored a Slack bot token.
    ///
    /// A memory-only store never fires, because nothing is hydrated from the OS.
    pub fn on_slack_token_hydrated(&self, hook: impl FnOnce() + Send + 'static) {
        let mut hook = Some(hook);
        let fire_now = {
            let Ok(mut inner) = self.lock() else {
                return;
            };
            match &mut inner.slack_hydrate {
                SlackHydrate::Waiting { hook: slot } => {
                    if let Some(hook) = hook.take() {
                        *slot = Some(Box::new(hook));
                    }
                    false
                }
                SlackHydrate::Settled { notify } => {
                    *notify && inner.slack.contains_key(&SlackSecretKey::BotToken)
                }
            }
        };
        if fire_now && let Some(hook) = hook.take() {
            hook();
        }
    }

    fn signal_slack_hydrated(&self) {
        let (hook, token_present) = {
            let Ok(mut inner) = self.lock() else {
                return;
            };
            let SlackHydrate::Waiting { hook } = &mut inner.slack_hydrate else {
                return;
            };
            let hook = hook.take();
            let token_present = inner.slack.contains_key(&SlackSecretKey::BotToken);
            inner.slack_hydrate = SlackHydrate::Settled {
                notify: token_present,
            };
            (hook, token_present)
        };
        if token_present && let Some(hook) = hook {
            hook();
        }
    }

    fn store_hydrated_slack(&self, value: Result<Vec<(SlackSecretKey, String)>, SecretError>) {
        let Ok(mut inner) = self.lock() else {
            return;
        };
        match value {
            Ok(pairs) => {
                for (key, raw) in pairs {
                    if inner.slack_dirty.contains(&key) {
                        continue;
                    }
                    let trimmed = raw.trim();
                    if trimmed.is_empty() {
                        inner.slack.remove(&key);
                    } else {
                        inner.slack.insert(key, trimmed.to_string());
                    }
                }
            }
            Err(error) => {
                tracing::warn!(error = %error, "slack install hydrate failed");
            }
        }
    }

    fn flush_slack(&self) -> Result<(), SecretError> {
        let snapshot = {
            let mut inner = self.lock()?;
            if inner.phase != AttachPhase::Ready || inner.slack_dirty.is_empty() {
                return Ok(());
            }
            let keys: Vec<SlackSecretKey> = inner.slack_dirty.drain().collect();
            keys.into_iter()
                .map(|key| (key, inner.slack.get(&key).cloned()))
                .collect::<Vec<_>>()
        };
        for (index, (key, value)) in snapshot.iter().enumerate() {
            let write = match value {
                Some(value) => slack_os_set(*key, value),
                None => slack_os_delete(*key),
            };
            if let Err(error) = write {
                if let Ok(mut inner) = self.lock() {
                    let pending: Vec<SlackSecretKey> =
                        snapshot[index..].iter().map(|(key, _)| *key).collect();
                    requeue_slack(&mut inner.slack_dirty, &pending);
                }
                return Err(error);
            }
        }
        Ok(())
    }

    fn flush_loop<F>(&self, mut commit: F) -> Result<(), SecretError>
    where
        F: FnMut(&FlushSnapshot) -> Result<(), SecretError>,
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

    fn take_flush_snapshot(&self) -> Result<Option<FlushSnapshot>, SecretError> {
        let mut inner = self.lock()?;
        if inner.phase != AttachPhase::Ready {
            inner.flush_in_flight = false;
            inner.flush_pending = false;
            return Ok(None);
        }
        inner.flush_pending = false;
        Ok(Some(
            SecretKey::PERSISTENT.map(|key| (key, inner.values.get(&key).cloned())),
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

type FlushSnapshot = [(SecretKey, Option<String>); 4];

impl SlackSecretVault for SecretStore {
    fn get_secret(&self, key: SlackSecretKey) -> Option<String> {
        self.lock().ok()?.slack.get(&key).cloned()
    }

    fn set_secret(&self, key: SlackSecretKey, value: &str) {
        let Ok(mut inner) = self.lock() else {
            return;
        };
        let trimmed = value.trim();
        if trimmed.is_empty() {
            inner.slack.remove(&key);
        } else {
            inner.slack.insert(key, trimmed.to_string());
        }
        if key.persist_to_os() {
            inner.slack_dirty.insert(key);
        }
    }
}

impl DiscordSecretVault for SecretStore {
    fn bot_token(&self) -> Option<String> {
        self.discord_bot_token().ok().flatten()
    }

    fn set_bot_token(&self, value: &str) {
        if let Err(error) = self.set_discord_bot_token(value) {
            tracing::warn!(error = %error, "discord bot token memory write failed");
        }
    }
}

impl TelegramSecretVault for SecretStore {
    fn get_secret(&self, key: TelegramSecretKey) -> Option<String> {
        self.get(key).ok().flatten()
    }

    fn secrets_hydrated(&self) -> bool {
        self.attach_settled()
    }

    fn set_secret(&self, key: TelegramSecretKey, value: &str) {
        if let Err(error) = self.set(key, value) {
            tracing::warn!(error = %error, "memory secret write failed");
        }
    }

    fn persists(&self) -> bool {
        self.persistence() != Persistence::ThisSession
    }

    fn tdlib_folder_name(&self) -> &'static str {
        let keyutils = self
            .lock()
            .is_ok_and(|inner| inner.os_backend == Some(OsBackend::KernelKeyring));
        if keyutils {
            TDLIB_KEYUTILS_FOLDER
        } else {
            TDLIB_FOLDER
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FlushAction {
    Spawn,
    Defer,
    Ignore,
}

/// Read every persistent key.
///
/// `Ok(None)` omits the key. That is a confirmed missing entry. `Err` drops
/// keys already read and returns the error, so a later key failure cannot be
/// applied as a partial `Ready` snapshot.
fn read_os_snapshot() -> Result<HashMap<SecretKey, String>, SecretError> {
    let mut os_values = HashMap::new();
    for key in SecretKey::PERSISTENT {
        match os_get(key) {
            Ok(Some(value)) => {
                os_values.insert(key, value);
            }
            Ok(None) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(os_values)
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

/// Why no OS store settled.
#[derive(Debug)]
enum AttachError {
    /// No usable OS store: memory-only for this session.
    Absent(SecretError),
    /// A store is present but failed (locked, prompt dismissed, timeout, D-Bus
    /// error). Its keys are unknown, never "missing": the UI offers Try again.
    ReadFailed(SecretError),
}

impl AttachError {
    fn into_secret_error(self) -> SecretError {
        match self {
            Self::Absent(error) | Self::ReadFailed(error) => error,
        }
    }
}

/// What the Linux attach does after Secret Service failed.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinuxFallback {
    /// Secret Service is not on this system (no provider, no D-Bus session).
    UseKeyutils,
    /// Secret Service is there but failed. Keyutils would show no keys and
    /// could make a saved session look lost, so it is not used.
    ReadFailed,
}

/// Only `secret_service::Error::Unavailable` means "not present". The
/// `secret-service` crate maps a missing bus address, bus socket, or provider
/// interface to it at connect time.
#[cfg(target_os = "linux")]
fn linux_fallback(error: &keyring_core::Error) -> LinuxFallback {
    let keyring_core::Error::PlatformFailure(inner) = error else {
        return LinuxFallback::ReadFailed;
    };
    match inner.downcast_ref::<secret_service::Error>() {
        Some(secret_service::Error::Unavailable) => LinuxFallback::UseKeyutils,
        Some(secret_service::Error::Zbus(zbus)) if no_bus_at_connect(zbus) => {
            LinuxFallback::UseKeyutils
        }
        _ => LinuxFallback::ReadFailed,
    }
}

/// zbus 5.19 reports a failed connect as `Connection(io::Error, Address)`.
/// `secret-service` 5.2 maps only the older `InputOutput(NotFound)` to
/// `Unavailable`, so a missing bus socket reaches us as `Zbus(..)` (#45).
/// Only a socket that is not there, or that refuses the connection, counts.
/// Other failures (a lock, a prompt, a timeout, a broken pipe on a live
/// session) stay `ReadFailed`.
#[cfg(target_os = "linux")]
fn no_bus_at_connect(error: &(dyn std::error::Error + 'static)) -> bool {
    use std::io::ErrorKind;
    let mut next = error.source();
    while let Some(source) = next {
        let io = source.downcast_ref::<std::io::Error>().or_else(|| {
            source
                .downcast_ref::<std::sync::Arc<std::io::Error>>()
                .map(AsRef::as_ref)
        });
        if let Some(io) = io {
            return matches!(
                io.kind(),
                ErrorKind::NotFound | ErrorKind::ConnectionRefused
            );
        }
        next = source.source();
    }
    false
}

fn probe_os() -> Result<OsBackend, AttachError> {
    if keyring_core::get_default_store().is_none() {
        let backend = install_platform_store()?;
        let _ = OS_BACKEND.set(backend);
        return Ok(backend);
    }
    probe_entry().map_err(|error| AttachError::ReadFailed(map_keyring_error(error)))?;
    Ok(OS_BACKEND.get().copied().unwrap_or(OsBackend::Native))
}

/// Read one entry. A missing entry proves the store works.
fn probe_entry() -> Result<(), keyring_core::Error> {
    let entry = Entry::new(TELEGRAM_SECRET_SERVICE, SecretKey::ApiId.account())?;
    match entry.get_password() {
        Ok(_) | Err(keyring_core::Error::NoEntry) => Ok(()),
        Err(error) => Err(error),
    }
}

fn ensure_default_store() -> Result<(), SecretError> {
    if keyring_core::get_default_store().is_some() {
        return Ok(());
    }
    let backend = install_platform_store().map_err(AttachError::into_secret_error)?;
    let _ = OS_BACKEND.set(backend);
    Ok(())
}

/// Secret Service first. Keyutils only when Secret Service is not present.
#[cfg(target_os = "linux")]
fn install_platform_store() -> Result<OsBackend, AttachError> {
    let [primary, fallback] = LINUX_BACKENDS;
    match try_linux_store(primary) {
        Ok(()) => return Ok(primary),
        Err(error) => {
            keyring_core::unset_default_store();
            match linux_fallback(&error) {
                LinuxFallback::UseKeyutils => {
                    tracing::info!(error = %error, "Secret Service is not present; using keyutils");
                }
                LinuxFallback::ReadFailed => {
                    return Err(AttachError::ReadFailed(map_keyring_error(error)));
                }
            }
        }
    }
    try_linux_store(fallback)
        .map(|()| fallback)
        .map_err(|error| {
            keyring_core::unset_default_store();
            AttachError::Absent(map_keyring_error(error))
        })
}

#[cfg(target_os = "linux")]
fn try_linux_store(backend: OsBackend) -> Result<(), keyring_core::Error> {
    match backend {
        OsBackend::SecretService => {
            keyring_core::set_default_store(zbus_secret_service_keyring_store::Store::new()?);
        }
        OsBackend::KernelKeyring | OsBackend::Native => {
            keyring_core::set_default_store(linux_keyutils_keyring_store::Store::new()?);
        }
    }
    probe_entry()
}

#[cfg(not(target_os = "linux"))]
fn install_platform_store() -> Result<OsBackend, AttachError> {
    let store = {
        #[cfg(target_os = "macos")]
        {
            apple_native_keyring_store::keychain::Store::new()
                .map_err(|error| AttachError::Absent(map_keyring_error(error)))?
        }
        #[cfg(windows)]
        {
            windows_native_keyring_store::Store::new()
                .map_err(|error| AttachError::Absent(map_keyring_error(error)))?
        }
        #[cfg(not(any(target_os = "macos", windows)))]
        {
            return Err(AttachError::Absent(SecretError::new(
                "OS keychain: unsupported platform",
            )));
        }
    };
    keyring_core::set_default_store(store);
    probe_entry().map_err(|error| {
        keyring_core::unset_default_store();
        AttachError::ReadFailed(map_keyring_error(error))
    })?;
    Ok(OsBackend::Native)
}

fn os_entry(key: SecretKey) -> Result<Entry, SecretError> {
    ensure_default_store()?;
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
        Err(keyring_core::Error::NoEntry) => Ok(None),
        Err(error) => Err(map_keyring_error(error)),
    }
}

fn discord_os_entry() -> Result<Entry, SecretError> {
    ensure_default_store()?;
    Entry::new(DISCORD_SECRET_SERVICE, DISCORD_SECRET_BOT_TOKEN).map_err(map_keyring_error)
}

fn read_discord_os_token() -> Result<Option<String>, SecretError> {
    match discord_os_entry()?.get_password() {
        Ok(value) => Ok(Some(value)),
        Err(keyring_core::Error::NoEntry) => Ok(None),
        Err(error) => Err(map_keyring_error(error)),
    }
}

fn discord_os_set(value: &str) -> Result<(), SecretError> {
    discord_os_entry()?
        .set_password(value)
        .map_err(map_keyring_error)
}

fn slack_os_entry(key: SlackSecretKey) -> Result<Entry, SecretError> {
    ensure_default_store()?;
    Entry::new(SLACK_SECRET_SERVICE, key.account()).map_err(map_keyring_error)
}

fn slack_os_get(key: SlackSecretKey) -> Result<Option<String>, SecretError> {
    match slack_os_entry(key)?.get_password() {
        Ok(value) => Ok(Some(value)),
        Err(keyring_core::Error::NoEntry) => Ok(None),
        Err(error) => Err(map_keyring_error(error)),
    }
}

fn requeue_slack(dirty: &mut HashSet<SlackSecretKey>, pending: &[SlackSecretKey]) {
    for key in pending {
        dirty.insert(*key);
    }
}

fn slack_os_set(key: SlackSecretKey, value: &str) -> Result<(), SecretError> {
    slack_os_entry(key)?
        .set_password(value)
        .map_err(map_keyring_error)
}

fn slack_os_delete(key: SlackSecretKey) -> Result<(), SecretError> {
    match slack_os_entry(key)?.delete_credential() {
        Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
        Err(error) => Err(map_keyring_error(error)),
    }
}

fn read_slack_os() -> Result<Vec<(SlackSecretKey, String)>, SecretError> {
    let mut found = Vec::new();
    for key in SlackSecretKey::PERSISTENT {
        if let Some(value) = slack_os_get(key)? {
            found.push((key, value));
        }
    }
    Ok(found)
}

fn discord_os_delete() -> Result<(), SecretError> {
    match discord_os_entry()?.delete_credential() {
        Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
        Err(error) => Err(map_keyring_error(error)),
    }
}

fn os_delete(key: SecretKey) -> Result<(), SecretError> {
    match os_entry(key)?.delete_credential() {
        Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
        Err(error) => Err(map_keyring_error(error)),
    }
}

/// Test hooks. Other workspace crates reach them through feature `test-support`.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
impl SecretStore {
    pub fn detached_for_test() -> Arc<Self> {
        Arc::new(Self::blank(AttachPhase::Detached))
    }

    pub fn fail_attach_for_test(&self) {
        if let Ok(mut inner) = self.lock() {
            inner.phase = AttachPhase::ReadFailed;
        }
    }

    pub fn complete_ready_attach_for_test(&self, os: &[(SecretKey, &str)]) {
        let values = os
            .iter()
            .map(|(key, value)| (*key, (*value).to_string()))
            .collect();
        let _ = self.finish_ready(values, None);
    }

    pub fn set_backend_for_test(&self, backend: OsBackend) {
        if let Ok(mut inner) = self.lock() {
            inner.os_backend = Some(backend);
        }
    }

    pub fn complete_discord_hydrate_for_test(&self, token: Option<&str>) {
        let value = match token {
            Some(token) => Ok(Some(token.to_string())),
            None => Ok(None),
        };
        self.store_hydrated_discord_token(value);
        self.signal_discord_hydrated();
    }
}

fn map_keyring_error(error: keyring_core::Error) -> SecretError {
    let kind = match error {
        keyring_core::Error::NoEntry => "not found",
        keyring_core::Error::NoStorageAccess(_) => "no storage access",
        keyring_core::Error::PlatformFailure(_) => "platform failure",
        keyring_core::Error::TooLong(_, _) => "value too long",
        keyring_core::Error::Invalid(_, _) => "invalid attribute",
        keyring_core::Error::Ambiguous(_) => "ambiguous entry",
        keyring_core::Error::NoDefaultStore => "no default store",
        keyring_core::Error::NotSupportedByStore(_) => "unsupported by store",
        other => {
            let _ = other;
            "keychain error"
        }
    };
    SecretError::new(format!("OS keychain: {kind}"))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

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
            self.finish_ready(os_values, None)
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
    fn discord_hydrate_hook_waits_until_a_token_is_stored() {
        let store = SecretStore::blank(AttachPhase::Detached);
        let (tx, rx) = std::sync::mpsc::channel();
        store.on_discord_token_hydrated(move || {
            tx.send(()).expect("hook");
        });
        assert!(rx.try_recv().is_err());
        store.store_hydrated_discord_token(Ok(None));
        store.signal_discord_hydrated();
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn discord_hydrate_hook_fires_after_the_token_is_stored() {
        let store = SecretStore::blank(AttachPhase::Detached);
        let (tx, rx) = std::sync::mpsc::channel();
        store.on_discord_token_hydrated(move || {
            tx.send(()).expect("hook");
        });
        store.store_hydrated_discord_token(Ok(Some("fixture-bot-token".into())));
        assert!(rx.try_recv().is_err());
        store.signal_discord_hydrated();
        assert!(rx.try_recv().is_ok());
        let debug = format!("{store:?}");
        assert!(!debug.contains("fixture-bot-token"));
    }

    #[test]
    fn discord_hydrate_hook_fires_when_registered_after_attach() {
        let store = SecretStore::detached_for_test();
        store.complete_discord_hydrate_for_test(Some("fixture-bot-token"));
        let (tx, rx) = std::sync::mpsc::channel();
        store.on_discord_token_hydrated(move || {
            tx.send(()).expect("hook");
        });
        assert!(rx.try_recv().is_ok());
        assert!(!format!("{store:?}").contains("fixture-bot-token"));
    }

    #[test]
    fn memory_only_store_does_not_emit_a_discord_hydrate_hook() {
        let store = SecretStore::memory();
        store
            .set_discord_bot_token("fixture-bot-token")
            .expect("memory token");
        let (tx, rx) = std::sync::mpsc::channel();
        store.on_discord_token_hydrated(move || {
            tx.send(()).expect("hook");
        });
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn discord_bot_token_stays_in_memory_and_out_of_debug() {
        let store = SecretStore::memory();
        store
            .set_discord_bot_token("  fixture-bot-token  ")
            .expect("set");
        assert_eq!(
            store.discord_bot_token().expect("get").as_deref(),
            Some("fixture-bot-token")
        );
        store.set_discord_bot_token(" ").expect("clear");
        assert_eq!(store.discord_bot_token().expect("cleared"), None);
        store
            .set_discord_bot_token("fixture-bot-token")
            .expect("set again");
        let debug = format!("{store:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("fixture-bot-token"));
        assert_eq!(DISCORD_SECRET_BOT_TOKEN, "discord.bot_token");
    }

    #[test]
    fn slack_install_stays_in_memory_and_out_of_debug() {
        let store = SecretStore::memory();
        SlackSecretVault::set_secret(&store, SlackSecretKey::BotToken, "  xoxb-fixture  ");
        SlackSecretVault::set_secret(&store, SlackSecretKey::TeamId, "T1");
        SlackSecretVault::set_secret(&store, SlackSecretKey::AppId, "A1");
        SlackSecretVault::set_secret(&store, SlackSecretKey::OAuthState, "one-time");
        assert_eq!(
            SlackSecretVault::get_secret(&store, SlackSecretKey::BotToken).as_deref(),
            Some("xoxb-fixture")
        );
        assert_eq!(
            SlackSecretVault::get_secret(&store, SlackSecretKey::OAuthState).as_deref(),
            Some("one-time")
        );
        SlackSecretVault::set_secret(&store, SlackSecretKey::BotToken, " ");
        assert_eq!(
            SlackSecretVault::get_secret(&store, SlackSecretKey::BotToken),
            None
        );
        let shown = format!("{store:?}");
        assert!(!shown.contains("xoxb-fixture"));
        assert!(!shown.contains("one-time"));
        assert!(!SlackSecretKey::OAuthState.persist_to_os());
        assert!(SlackSecretKey::BotToken.persist_to_os());
        assert_eq!(store.request_flush(), FlushAction::Ignore);
    }

    #[test]
    fn a_failed_slack_flush_requeues_every_unwritten_key() {
        let mut dirty = HashSet::new();
        let pending = [
            SlackSecretKey::BotToken,
            SlackSecretKey::TeamId,
            SlackSecretKey::ClientId,
        ];
        requeue_slack(&mut dirty, &pending);
        assert!(dirty.contains(&SlackSecretKey::BotToken));
        assert!(dirty.contains(&SlackSecretKey::TeamId));
        assert!(dirty.contains(&SlackSecretKey::ClientId));
    }

    #[test]
    fn a_failed_slack_keychain_read_leaves_attach_unsettled() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let store = SecretStore::blank(AttachPhase::Attaching);
        let fired = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&fired);
        store.on_slack_token_hydrated(move || flag.store(true, Ordering::Relaxed));
        let mut telegram = HashMap::new();
        telegram.insert(SecretKey::Session, "saved-session".to_string());
        store.store_probed_secrets(
            OsBackend::SecretService,
            Ok(telegram),
            Ok(None),
            Err(SecretError::new("OS keychain: platform failure")),
        );
        assert_eq!(store.phase(), AttachPhase::ReadFailed);
        assert!(store.read_failed());
        assert!(!store.attach_settled());
        assert!(!fired.load(Ordering::Relaxed));
        assert_eq!(
            SlackSecretVault::get_secret(&store, SlackSecretKey::BotToken),
            None
        );
        assert_eq!(store.get(SecretKey::Session).expect("get"), None);

        let mut telegram = HashMap::new();
        telegram.insert(SecretKey::Session, "saved-session".to_string());
        store.store_probed_secrets(
            OsBackend::SecretService,
            Ok(telegram),
            Ok(None),
            Ok(vec![(SlackSecretKey::BotToken, "xoxb-fixture".to_string())]),
        );
        assert!(store.attach_settled());
        assert!(fired.load(Ordering::Relaxed));
        assert_eq!(
            SlackSecretVault::get_secret(&store, SlackSecretKey::BotToken).as_deref(),
            Some("xoxb-fixture")
        );
    }

    #[test]
    fn ephemeral_keys_are_memory_only() {
        let store = SecretStore::memory();
        store.set(SecretKey::Phone, "+15551234567").expect("phone");
        store.set(SecretKey::Code, "12345").expect("code");
        store.set(SecretKey::Password, "2fa-secret").expect("2fa");
        assert!(!SecretKey::Phone.persist_to_os());
        assert!(!SecretKey::Code.persist_to_os());
        assert!(!SecretKey::Password.persist_to_os());
        assert_eq!(store.request_flush(), FlushAction::Ignore);
        let debug = format!("{store:?}");
        assert!(!debug.contains("+15551234567"));
        assert!(!debug.contains("12345"));
        assert!(!debug.contains("2fa-secret"));
    }

    #[test]
    fn empty_value_deletes_key() {
        let store = SecretStore::memory();
        store.set(SecretKey::Session, "keep").expect("set");
        store.set(SecretKey::Session, "  ").expect("clear");
        assert_eq!(store.get(SecretKey::Session).expect("get"), None);
    }

    #[test]
    fn only_a_memory_only_store_asks_for_a_throwaway_tdlib_folder() {
        assert!(!TelegramSecretVault::persists(&SecretStore::memory()));
        let ready = SecretStore::blank(AttachPhase::Detached);
        assert!(
            TelegramSecretVault::persists(&ready),
            "loading counts as saved"
        );
        ready.complete_ready_attach_for_test(&[]);
        ready.set_backend_for_test(OsBackend::KernelKeyring);
        assert!(
            TelegramSecretVault::persists(&ready),
            "keyutils keeps the key until restart; the stale-folder move covers that"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn only_a_missing_secret_service_falls_back_to_keyutils() {
        use keyring_core::Error as KeyringError;
        let absent = KeyringError::PlatformFailure(Box::new(secret_service::Error::Unavailable));
        assert_eq!(linux_fallback(&absent), LinuxFallback::UseKeyutils);
        for failing in [
            KeyringError::NoStorageAccess(Box::new(secret_service::Error::Locked)),
            KeyringError::NoStorageAccess(Box::new(secret_service::Error::Prompt)),
            KeyringError::PlatformFailure(Box::new(secret_service::Error::PromptDisconnected)),
            KeyringError::PlatformFailure(Box::new(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "D-Bus call timed out",
            ))),
            KeyringError::NoEntry,
        ] {
            assert_eq!(
                linux_fallback(&failing),
                LinuxFallback::ReadFailed,
                "{failing:?} must not look like missing keys (Codex 4091651519)"
            );
        }
        let src = include_str!("secrets.rs");
        let install = &src[src
            .find("fn install_platform_store() -> Result<OsBackend, AttachError> {")
            .expect("linux install")..];
        let read_failed = install
            .find("return Err(AttachError::ReadFailed(")
            .expect("fail closed");
        let keyutils = install.find("try_linux_store(fallback)").expect("fallback");
        assert!(
            read_failed < keyutils,
            "a failing Secret Service never reaches keyutils"
        );
    }

    /// Set in the child process of
    /// `a_missing_dbus_session_falls_back_to_keyutils`.
    #[cfg(target_os = "linux")]
    const NO_BUS_CHILD_ENV: &str = "THINWIRE_TEST_NO_BUS_CHILD";

    /// With no D-Bus session, the real keyring store must reach
    /// `linux_fallback` as "not present", so keyutils is used (#45). The
    /// keyring wrapper keeps the inner `secret_service::Error` for the
    /// downcast. zbus 5.19 reports the missing socket as `Zbus(Connection)`,
    /// not as `Unavailable`; `no_bus_at_connect` covers that.
    /// The check runs in a child process: the bad bus address is set only
    /// there, and no other test thread sees it.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_missing_dbus_session_falls_back_to_keyutils() {
        const NAME: &str = "secrets::tests::a_missing_dbus_session_falls_back_to_keyutils";
        if std::env::var_os(NO_BUS_CHILD_ENV).is_some() {
            let error = try_linux_store(OsBackend::SecretService)
                .expect_err("no D-Bus session: Secret Service cannot open");
            keyring_core::unset_default_store();
            assert!(
                matches!(&error, keyring_core::Error::PlatformFailure(_)),
                "{error:?}"
            );
            assert_eq!(
                linux_fallback(&error),
                LinuxFallback::UseKeyutils,
                "no bus means no Secret Service: {error:?}"
            );
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args(["--exact", NAME, "--test-threads=1"])
            .env(NO_BUS_CHILD_ENV, "1")
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                "unix:path=/nonexistent/thinwire-test/bus",
            )
            .env_remove(KEYRING_ENV)
            .output()
            .expect("run the child test");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "child failed:\n{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            stdout.contains("1 passed"),
            "the child ran the check: {stdout}"
        );
    }

    #[test]
    fn a_keyutils_store_uses_its_own_tdlib_folder() {
        let store = SecretStore::blank(AttachPhase::Detached);
        store.complete_ready_attach_for_test(&[]);
        store.set_backend_for_test(OsBackend::SecretService);
        assert_eq!(TelegramSecretVault::tdlib_folder_name(&store), TDLIB_FOLDER);
        store.set_backend_for_test(OsBackend::KernelKeyring);
        assert_eq!(
            TelegramSecretVault::tdlib_folder_name(&store),
            TDLIB_KEYUTILS_FOLDER,
            "a keyutils lost-key move can never touch the Secret Service session"
        );
        assert_ne!(TDLIB_FOLDER, TDLIB_KEYUTILS_FOLDER);
    }

    #[test]
    fn linux_tries_secret_service_before_keyutils_and_only_keyutils_expires() {
        #[cfg(target_os = "linux")]
        assert_eq!(
            LINUX_BACKENDS,
            [OsBackend::SecretService, OsBackend::KernelKeyring]
        );
        assert!(OsBackend::SecretService.survives_restart());
        assert!(OsBackend::Native.survives_restart());
        assert!(!OsBackend::KernelKeyring.survives_restart());
        assert_eq!(
            SecretStore::memory().persistence(),
            Persistence::ThisSession
        );
        assert_eq!(
            SecretStore::blank(AttachPhase::Detached).persistence(),
            Persistence::Loading
        );
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

    /// Mirrors the TDLib keyless check: a settled, persisting vault with no
    /// database key. A hydrate error must not satisfy it.
    fn keyless_recovery_allowed(store: &SecretStore) -> bool {
        TelegramSecretVault::secrets_hydrated(store)
            && TelegramSecretVault::persists(store)
            && TelegramSecretVault::get_secret(store, SecretKey::DbEncryption).is_none()
    }

    #[test]
    fn hydrate_error_does_not_finish_ready_with_a_missing_db_key() {
        let store = SecretStore::blank(AttachPhase::Attaching);
        let mut partial = HashMap::new();
        partial.insert(SecretKey::Session, "saved-session".to_string());
        partial.insert(SecretKey::ApiId, "12345".to_string());
        let should_flush = store.settle_after_probe(
            OsBackend::SecretService,
            partial,
            Some(SecretError::new("OS keychain: platform failure")),
        );
        assert!(!should_flush);
        assert_eq!(store.phase(), AttachPhase::ReadFailed);
        assert!(
            store.read_failed(),
            "the UI offers Try again, not a spinner"
        );
        assert!(!store.attach_settled());
        assert!(!TelegramSecretVault::secrets_hydrated(&store));
        assert_eq!(store.get(SecretKey::Session).expect("session"), None);
        assert_eq!(store.get(SecretKey::DbEncryption).expect("db key"), None);
        assert!(
            !keyless_recovery_allowed(&store),
            "a read error must not move a valid TDLib folder aside"
        );
    }

    #[test]
    fn a_failed_read_defers_writes_and_a_retry_settles_the_store() {
        let store = SecretStore::blank(AttachPhase::Detached);
        store.fail_attach_for_test();
        assert!(store.read_failed());
        assert_eq!(store.persistence(), Persistence::Loading);
        assert_eq!(
            store.request_flush(),
            FlushAction::Defer,
            "no write on a failed read"
        );
        store.complete_ready_attach_for_test(&[(SecretKey::DbEncryption, "saved-key")]);
        assert!(!store.read_failed());
        assert!(store.attach_settled());
        assert_eq!(
            store.get(SecretKey::DbEncryption).expect("get").as_deref(),
            Some("saved-key")
        );
    }

    #[test]
    fn confirmed_missing_db_key_allows_keyless_recovery() {
        let store = SecretStore::blank(AttachPhase::Attaching);
        let mut os_values = HashMap::new();
        os_values.insert(SecretKey::Session, "saved-session".to_string());
        let should_flush = store.settle_after_probe(OsBackend::SecretService, os_values, None);
        assert!(!should_flush);
        assert_eq!(store.phase(), AttachPhase::Ready);
        assert!(store.attach_settled());
        assert!(TelegramSecretVault::secrets_hydrated(&store));
        assert!(TelegramSecretVault::persists(&store));
        assert_eq!(
            store.get(SecretKey::Session).expect("session").as_deref(),
            Some("saved-session")
        );
        assert_eq!(
            TelegramSecretVault::get_secret(&store, SecretKey::DbEncryption),
            None,
            "Ok(None) stays a confirmed missing entry"
        );
        assert!(keyless_recovery_allowed(&store));
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

    fn snapshot_value(snapshot: &FlushSnapshot, key: SecretKey) -> Option<String> {
        snapshot
            .iter()
            .find(|(item, _)| *item == key)
            .and_then(|(_, value)| value.clone())
    }

    #[derive(Clone)]
    struct StallSink {
        commits: Arc<Mutex<Vec<FlushSnapshot>>>,
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

        fn commits(&self) -> Vec<FlushSnapshot> {
            self.commits.lock().expect("commits").clone()
        }

        fn commit(&self, snapshot: &FlushSnapshot) -> Result<(), SecretError> {
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

    #[test]
    fn retry_leaves_read_failed_before_the_worker_runs() {
        let store = SecretStore::detached_for_test();
        store.fail_attach_for_test();
        assert!(store.read_failed());
        store.leave_read_failed();
        assert!(!store.read_failed(), "a new watch does not stop at once");
        assert!(!store.attach_settled());
        assert_eq!(store.persistence(), Persistence::Loading);
    }
}
