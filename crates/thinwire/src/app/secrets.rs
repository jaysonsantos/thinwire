//! OS secret store for Telegram `api_id`, `api_hash`, and session material.
//!
//! The UI thread only touches an in-memory map. OS keychain I/O runs on a
//! tokio `spawn_blocking` worker so egui never waits on keyutils / Keychain /
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
    DISCORD_SECRET_BOT_TOKEN, DISCORD_SECRET_SERVICE, DiscordSecretVault, TELEGRAM_SECRET_SERVICE,
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
    flush_pending: bool,
    flush_in_flight: bool,
    phase: AttachPhase,
    os_backend: Option<OsBackend>,
    discord_hydrate: DiscordHydrate,
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
                flush_pending: false,
                flush_in_flight: false,
                phase,
                os_backend: None,
                discord_hydrate: if phase == AttachPhase::MemoryOnly {
                    DiscordHydrate::Settled { notify: false }
                } else {
                    DiscordHydrate::Waiting { hook: None }
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
            return;
        }
        match probe_os() {
            Ok(backend) => {
                let discord_token = read_discord_os_token();
                let should_flush = match read_os_snapshot() {
                    Ok(os_values) => self.settle_after_probe(backend, os_values, None),
                    Err(error) => self.settle_after_probe(backend, HashMap::new(), Some(error)),
                };
                self.store_hydrated_discord_token(discord_token);
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
            }
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "OS keychain unavailable; Telegram secrets stay in memory this session and are not written to disk"
                );
                self.finish_memory_only();
                self.signal_discord_hydrated();
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
            if let Ok(mut inner) = self.lock() {
                inner.phase = AttachPhase::ReadFailed;
            }
            return false;
        }
        let should_flush = self.finish_ready(os_values);
        if let Ok(mut inner) = self.lock() {
            inner.os_backend = Some(backend);
        }
        should_flush
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
        let should_flush =
            inner.flush_pending || !inner.dirty.is_empty() || inner.discord_token_dirty;
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
        self.flush_discord_token()
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

fn probe_os() -> Result<OsBackend, SecretError> {
    ensure_default_store()?;
    probe_entry()?;
    Ok(OS_BACKEND.get().copied().unwrap_or(OsBackend::Native))
}

/// Read one entry. A missing entry proves the store works.
fn probe_entry() -> Result<(), SecretError> {
    let entry = Entry::new(TELEGRAM_SECRET_SERVICE, SecretKey::ApiId.account())
        .map_err(map_keyring_error)?;
    match entry.get_password() {
        Ok(_) | Err(keyring_core::Error::NoEntry) => Ok(()),
        Err(error) => Err(map_keyring_error(error)),
    }
}

fn ensure_default_store() -> Result<(), SecretError> {
    if keyring_core::get_default_store().is_some() {
        return Ok(());
    }
    let backend = install_platform_store()?;
    let _ = OS_BACKEND.set(backend);
    Ok(())
}

#[cfg(target_os = "linux")]
fn install_platform_store() -> Result<OsBackend, SecretError> {
    let mut last_error = SecretError::new("OS keychain: no Linux backend");
    for backend in LINUX_BACKENDS {
        match install_linux_store(backend).and_then(|()| probe_entry()) {
            Ok(()) => return Ok(backend),
            Err(error) => {
                tracing::info!(?backend, error = %error, "keychain backend not usable; trying the next one");
                keyring_core::unset_default_store();
                last_error = error;
            }
        }
    }
    Err(last_error)
}

#[cfg(target_os = "linux")]
fn install_linux_store(backend: OsBackend) -> Result<(), SecretError> {
    match backend {
        OsBackend::SecretService => keyring_core::set_default_store(
            zbus_secret_service_keyring_store::Store::new().map_err(map_keyring_error)?,
        ),
        OsBackend::KernelKeyring | OsBackend::Native => keyring_core::set_default_store(
            linux_keyutils_keyring_store::Store::new().map_err(map_keyring_error)?,
        ),
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn install_platform_store() -> Result<OsBackend, SecretError> {
    let store = {
        #[cfg(target_os = "macos")]
        {
            apple_native_keyring_store::keychain::Store::new().map_err(map_keyring_error)?
        }
        #[cfg(windows)]
        {
            windows_native_keyring_store::Store::new().map_err(map_keyring_error)?
        }
        #[cfg(not(any(target_os = "macos", windows)))]
        {
            return Err(SecretError::new("OS keychain: unsupported platform"));
        }
    };
    keyring_core::set_default_store(store);
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

#[cfg(test)]
impl SecretStore {
    pub(crate) fn detached_for_test() -> Arc<Self> {
        Arc::new(Self::blank(AttachPhase::Detached))
    }

    pub(crate) fn fail_attach_for_test(&self) {
        if let Ok(mut inner) = self.lock() {
            inner.phase = AttachPhase::ReadFailed;
        }
    }

    pub(crate) fn complete_ready_attach_for_test(&self, os: &[(SecretKey, &str)]) {
        let values = os
            .iter()
            .map(|(key, value)| (*key, (*value).to_string()))
            .collect();
        let _ = self.finish_ready(values);
    }

    pub(crate) fn set_backend_for_test(&self, backend: OsBackend) {
        if let Ok(mut inner) = self.lock() {
            inner.os_backend = Some(backend);
        }
    }

    pub(crate) fn complete_discord_hydrate_for_test(&self, token: Option<&str>) {
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
        assert_eq!(store.request_flush(), FlushAction::Ignore);
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
}
