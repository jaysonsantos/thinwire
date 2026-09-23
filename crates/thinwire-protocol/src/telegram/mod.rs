//! Telegram adapter on the official TDLib path (`tdlib-rs`).
//!
//! Default builds keep a compile-safe auth state machine so CI does not need
//! system TDLib. Enable `telegram-tdlib` to compile the live client. Read
//! credentials from [`TelegramSecretVault`] — never put them on commands, never
//! log them, never commit them.

mod credentials;
mod db_key;
mod engine;
mod inbox;

#[cfg(feature = "telegram-tdlib")]
mod tdlib;

use std::fmt;
use std::sync::Arc;

use tokio::sync::mpsc::UnboundedSender;

use engine::TelegramAuthEngine;

pub use credentials::{
    TelegramApiOrigin, TelegramApiSource, resolve_telegram_api, telegram_api_available,
};
pub use inbox::parse_telegram_chat_id;

use super::adapter::{
    AdapterCommand, AdapterError, AdapterStatus, EventTx, ProtocolAdapter, ProtocolCapabilities,
    ProtocolId, SupportClass, TelegramAuthPhase, TelegramAuthStep, emit_flush_secrets, emit_status,
    emit_telegram_auth,
};
use super::secrets::TelegramSecretVault;

#[cfg(feature = "telegram-tdlib")]
const CAPABILITY_DETAIL: &str =
    "Official TDLib via Rust bindings (tdlib-rs). Supported goal. Live client compiled.";

#[cfg(not(feature = "telegram-tdlib"))]
const CAPABILITY_DETAIL: &str = "Official TDLib via Rust bindings (tdlib-rs). Supported goal. TDLib unavailable in this build; enable feature telegram-tdlib after a local TDLib install.";

const CAPABILITIES: ProtocolCapabilities = ProtocolCapabilities {
    id: ProtocolId::Telegram,
    support: SupportClass::Supported,
    short_label: "Supported · TDLib",
    detail: CAPABILITY_DETAIL,
    official_api: true,
    allows_user_account_automation: false,
};

/// Official Telegram path. Default builds report TDLib unavailable.
pub struct TelegramAdapter {
    secrets: Arc<dyn TelegramSecretVault>,
    api_source: TelegramApiSource,
    engine: TelegramAuthEngine,
    #[cfg(feature = "telegram-tdlib")]
    tdlib: tdlib::TdlibRuntime,
}

impl TelegramAdapter {
    #[must_use]
    pub fn new(secrets: Arc<dyn TelegramSecretVault>) -> Self {
        Self::with_source(secrets, TelegramApiSource::from_build())
    }

    #[must_use]
    pub fn with_source(
        secrets: Arc<dyn TelegramSecretVault>,
        api_source: TelegramApiSource,
    ) -> Self {
        Self {
            secrets,
            api_source,
            engine: TelegramAuthEngine::new(),
            #[cfg(feature = "telegram-tdlib")]
            tdlib: tdlib::TdlibRuntime::new(),
        }
    }

    /// Test helper that owns a process-local vault.
    #[must_use]
    pub fn memory() -> Self {
        Self::new(Arc::new(super::secrets::MemorySecretVault::new()))
    }

    #[must_use]
    pub const fn capabilities() -> ProtocolCapabilities {
        CAPABILITIES
    }

    /// True when the `telegram-tdlib` feature compiled the live client.
    #[must_use]
    pub const fn uses_tdlib_hook() -> bool {
        uses_tdlib_hook()
    }

    #[must_use]
    pub fn backend_detail() -> &'static str {
        if uses_tdlib_hook() {
            "tdlib-rs live client compiled; FFI and network I/O stay off the UI thread"
        } else {
            "TDLib unavailable — enable feature telegram-tdlib after a local TDLib install; never put api_id/api_hash in the repo"
        }
    }

    fn handle_auth(
        &mut self,
        step: TelegramAuthStep,
        events: &EventTx,
    ) -> Result<(), AdapterError> {
        let phase = match self
            .engine
            .submit(step, self.secrets.as_ref(), &self.api_source)
        {
            Ok(phase) => phase,
            Err(error) => {
                emit_telegram_auth(events, TelegramAuthPhase::Failed);
                return Err(error);
            }
        };
        #[cfg(feature = "telegram-tdlib")]
        {
            self.tdlib.submit(
                step,
                Arc::clone(&self.secrets),
                self.api_source.clone(),
                events,
            );
            let _ = phase;
            Ok(())
        }
        #[cfg(not(feature = "telegram-tdlib"))]
        {
            emit_telegram_auth(events, phase);
            emit_status(
                events,
                ProtocolId::Telegram,
                phase_status(phase),
                phase_detail(phase, Self::backend_detail()),
            );
            Ok(())
        }
    }
}

impl Default for TelegramAdapter {
    fn default() -> Self {
        Self::memory()
    }
}

impl fmt::Debug for TelegramAdapter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TelegramAdapter")
            .field("tdlib", &uses_tdlib_hook())
            .field("api_source", &self.api_source)
            .field("last_phase", &self.engine.last_phase())
            .finish()
    }
}

impl ProtocolAdapter for TelegramAdapter {
    fn id(&self) -> ProtocolId {
        ProtocolId::Telegram
    }

    fn capabilities(&self) -> ProtocolCapabilities {
        CAPABILITIES
    }

    fn start(&mut self, events: EventTx) {
        tracing::info!(backend = Self::backend_detail(), "telegram adapter start");
        emit_status(
            &events,
            ProtocolId::Telegram,
            if uses_tdlib_hook() {
                AdapterStatus::Connecting
            } else {
                AdapterStatus::Stubbed
            },
            Self::backend_detail(),
        );
    }

    fn handle(&mut self, command: AdapterCommand, events: &EventTx) -> Result<(), AdapterError> {
        match command {
            AdapterCommand::TelegramAuth { step } => self.handle_auth(step, events),
            AdapterCommand::Connect {
                protocol: ProtocolId::Telegram,
            } => {
                emit_status(
                    events,
                    ProtocolId::Telegram,
                    if uses_tdlib_hook() {
                        AdapterStatus::Connecting
                    } else {
                        AdapterStatus::Stubbed
                    },
                    Self::backend_detail(),
                );
                Ok(())
            }
            AdapterCommand::Disconnect {
                protocol: ProtocolId::Telegram,
            } => {
                self.engine.reset();
                #[cfg(feature = "telegram-tdlib")]
                self.tdlib.stop();
                emit_status(
                    events,
                    ProtocolId::Telegram,
                    AdapterStatus::Stubbed,
                    "Telegram disconnected.",
                );
                Ok(())
            }
            AdapterCommand::LoadChats {
                protocol: ProtocolId::Telegram,
            } => self.dispatch_live(events, LiveCall::LoadChats),
            AdapterCommand::OpenChat {
                protocol: ProtocolId::Telegram,
                conversation_id,
            } => {
                if parse_telegram_chat_id(&conversation_id).is_none() {
                    return Err(AdapterError::Unavailable {
                        protocol: ProtocolId::Telegram,
                        reason: "chat id is not a Telegram chat",
                    });
                }
                self.dispatch_live(events, LiveCall::OpenChat(conversation_id))
            }
            AdapterCommand::SendText {
                protocol: ProtocolId::Telegram,
                conversation_id,
                body,
            } => {
                if body.trim().is_empty() {
                    return Err(AdapterError::Unavailable {
                        protocol: ProtocolId::Telegram,
                        reason: "message text is empty",
                    });
                }
                if parse_telegram_chat_id(&conversation_id).is_none() {
                    return Err(AdapterError::Unavailable {
                        protocol: ProtocolId::Telegram,
                        reason: "chat id is not a Telegram chat",
                    });
                }
                self.dispatch_live(
                    events,
                    LiveCall::SendText {
                        conversation_id,
                        body,
                    },
                )
            }
            other => Err(AdapterError::Unavailable {
                protocol: ProtocolId::Telegram,
                reason: command_mismatch(other),
            }),
        }
    }
}

#[must_use]
pub const fn uses_tdlib_hook() -> bool {
    cfg!(feature = "telegram-tdlib")
}

fn command_mismatch(_command: AdapterCommand) -> &'static str {
    "command is not handled by the Telegram adapter"
}

#[cfg(not(feature = "telegram-tdlib"))]
const fn phase_status(phase: TelegramAuthPhase) -> AdapterStatus {
    match phase {
        TelegramAuthPhase::Ready => AdapterStatus::Ready,
        TelegramAuthPhase::Unavailable => AdapterStatus::Stubbed,
        TelegramAuthPhase::Failed => AdapterStatus::Error,
        TelegramAuthPhase::NeedPhone
        | TelegramAuthPhase::NeedCode
        | TelegramAuthPhase::NeedTwoFactor => AdapterStatus::Connecting,
    }
}

#[cfg(not(feature = "telegram-tdlib"))]
fn phase_detail(phase: TelegramAuthPhase, backend: &str) -> String {
    format!(
        "Telegram {phase} queued. Credentials stay in the secret store. {backend}",
        phase = phase.as_str()
    )
}

#[cfg_attr(not(feature = "telegram-tdlib"), allow(dead_code))]
enum LiveCall {
    LoadChats,
    OpenChat(String),
    SendText {
        conversation_id: String,
        body: String,
    },
}

impl TelegramAdapter {
    fn dispatch_live(&mut self, events: &EventTx, call: LiveCall) -> Result<(), AdapterError> {
        #[cfg(feature = "telegram-tdlib")]
        {
            let secrets = Arc::clone(&self.secrets);
            let source = self.api_source.clone();
            match call {
                LiveCall::LoadChats => self.tdlib.load_chats(secrets, source, events),
                LiveCall::OpenChat(conversation_id) => {
                    self.tdlib
                        .open_chat(conversation_id, secrets, source, events);
                }
                LiveCall::SendText {
                    conversation_id,
                    body,
                } => {
                    self.tdlib
                        .send_text(conversation_id, body, secrets, source, events);
                }
            }
            Ok(())
        }
        #[cfg(not(feature = "telegram-tdlib"))]
        {
            let _ = (events, call);
            Err(AdapterError::Unavailable {
                protocol: ProtocolId::Telegram,
                reason: "TDLib unavailable in this build",
            })
        }
    }
}

/// Persist vault keys via the UI keychain-flush path. Never puts values on the event.
#[cfg_attr(not(feature = "telegram-tdlib"), allow(dead_code))]
pub(crate) fn request_secret_flush(events: &EventTx) {
    emit_flush_secrets(events);
}

/// Send `message`, spawning a worker if the slot is empty. On a dead sender,
/// clear the slot, spawn once more, and retry the send. Returns `false` only
/// if both attempts fail (slot is then `None`).
#[cfg_attr(not(feature = "telegram-tdlib"), allow(dead_code))]
pub(crate) fn send_or_respawn<T: Clone>(
    slot: &mut Option<UnboundedSender<T>>,
    message: T,
    mut spawn: impl FnMut() -> UnboundedSender<T>,
) -> bool {
    if slot.is_none() {
        *slot = Some(spawn());
    }
    if slot
        .as_ref()
        .is_some_and(|tx| tx.send(message.clone()).is_ok())
    {
        return true;
    }
    *slot = None;
    *slot = Some(spawn());
    if slot.as_ref().is_some_and(|tx| tx.send(message).is_ok()) {
        return true;
    }
    *slot = None;
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AdapterEvent;
    use crate::secrets::{MemorySecretVault, TelegramSecretKey};

    #[test]
    fn default_build_uses_compile_safe_unavailable_path() {
        assert_eq!(
            TelegramAdapter::uses_tdlib_hook(),
            cfg!(feature = "telegram-tdlib")
        );
        assert!(
            TelegramAdapter::backend_detail().contains("TDLib")
                || TelegramAdapter::uses_tdlib_hook()
        );
    }

    #[test]
    fn capabilities_mark_telegram_supported_official() {
        let caps = TelegramAdapter::capabilities();
        assert_eq!(caps.support, SupportClass::Supported);
        assert!(caps.official_api);
        assert!(caps.short_label.contains("TDLib"));
        assert!(!caps.detail.to_ascii_lowercase().contains("reliable"));
    }

    #[test]
    fn telegram_auth_step_does_not_echo_secrets() {
        let vault = Arc::new(MemorySecretVault::new());
        vault.set_secret(TelegramSecretKey::ApiId, "11111");
        vault.set_secret(TelegramSecretKey::ApiHash, "hash-value");
        let mut adapter = TelegramAdapter::new(Arc::clone(&vault) as Arc<dyn TelegramSecretVault>);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        adapter
            .handle(
                AdapterCommand::TelegramAuth {
                    step: TelegramAuthStep::ApiCredentials,
                },
                &tx,
            )
            .expect("auth step");
        let mut saw_phase = false;
        while let Ok(event) = rx.try_recv() {
            match event {
                AdapterEvent::TelegramAuth { phase } => {
                    assert_eq!(phase, TelegramAuthPhase::NeedPhone);
                    saw_phase = true;
                }
                AdapterEvent::Status { detail, .. } => {
                    assert!(detail.contains("secret store") || detail.contains("TDLib"));
                    assert!(!detail.contains("11111"));
                    assert!(!detail.contains("hash-value"));
                }
                other => panic!("unexpected event {other:?}"),
            }
        }
        if !uses_tdlib_hook() {
            assert!(saw_phase, "unavailable path must emit NeedPhone");
        }
        let debug = format!(
            "{:?}",
            AdapterCommand::TelegramAuth {
                step: TelegramAuthStep::ApiCredentials
            }
        );
        assert!(debug.contains("ApiCredentials"));
        assert!(!debug.contains("11111"));
        assert!(!debug.contains("hash-value"));
        let adapter_debug = format!("{adapter:?}");
        assert!(!adapter_debug.contains("11111"));
        assert!(!adapter_debug.contains("hash-value"));
    }

    #[test]
    fn unavailable_state_machine_covers_every_messenger_ux_step() {
        if uses_tdlib_hook() {
            return;
        }
        let vault = Arc::new(MemorySecretVault::new());
        vault.set_secret(TelegramSecretKey::ApiId, "11111");
        vault.set_secret(TelegramSecretKey::ApiHash, "hash-value");
        vault.set_secret(TelegramSecretKey::Phone, "+15551234567");
        vault.set_secret(TelegramSecretKey::Code, "12345");
        vault.set_secret(TelegramSecretKey::Password, "2fa-secret");
        let mut adapter = TelegramAdapter::new(Arc::clone(&vault) as Arc<dyn TelegramSecretVault>);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        let steps = [
            (
                TelegramAuthStep::ApiCredentials,
                TelegramAuthPhase::NeedPhone,
            ),
            (TelegramAuthStep::Phone, TelegramAuthPhase::NeedCode),
            (TelegramAuthStep::Code, TelegramAuthPhase::NeedTwoFactor),
            (TelegramAuthStep::TwoFactor, TelegramAuthPhase::Unavailable),
        ];
        for (step, expected) in steps {
            adapter
                .handle(AdapterCommand::TelegramAuth { step }, &tx)
                .expect("step");
            let event = rx.try_recv().expect("phase event");
            match event {
                AdapterEvent::TelegramAuth { phase } => assert_eq!(phase, expected),
                other => panic!("expected phase, got {other:?}"),
            }
            let AdapterEvent::Status { detail, .. } = rx.try_recv().expect("status") else {
                panic!("expected status");
            };
            assert!(!detail.contains("11111"));
            assert!(!detail.contains("hash-value"));
            assert!(!detail.contains("+15551234567"));
            assert!(!detail.contains("12345"));
            assert!(!detail.contains("2fa-secret"));
        }
    }

    #[test]
    fn live_tdlib_loads_chats_and_messages_after_ready() {
        let src = include_str!("tdlib.rs");
        let ready = src.find("AuthorizationState::Ready").expect("ready arm");
        let after_ready = &src[ready..];
        assert!(after_ready.contains("load_main_chats"));
        assert!(src.contains("functions::load_chats"));
        assert!(src.contains("functions::get_chat_history"));
        assert!(src.contains("functions::send_message"));
        assert!(src.contains("live.authorized = true"));
        assert!(!src.contains("telegram:ready"));
        assert!(!src.contains("while let Some"));
        assert!(!src.contains(".phone_number"));
        assert!(src.contains("thinwire-tdlib-recv"));
        let send = fn_body(src, "async fn send_text");
        assert!(
            !send.contains("Message sent."),
            "send_message must not announce success before TDLib confirms it"
        );
        let updates = fn_body(src, "fn apply_chat_update");
        assert!(updates.contains("Update::MessageSendSucceeded"));
        assert!(updates.contains("Message sent."));
        assert!(updates.contains("Update::MessageSendFailed"));
        assert!(updates.contains("Update::DeleteMessages"));
        assert!(updates.contains("update.from_cache"));
        assert!(updates.contains("emit_messages_removed"));
        assert!(updates.contains("set_preview(update.chat_id, \"\")"));
        let open = fn_body(src, "async fn open_chat");
        assert!(open.contains("functions::view_messages"));
        assert!(open.contains("true,"));
    }

    #[test]
    fn live_tdlib_drops_a_stale_session_marker_on_phone_prompt() {
        let src = include_str!("tdlib.rs");
        let auth = fn_body(src, "async fn apply_authorization");
        let start = auth
            .find("AuthorizationState::WaitPhoneNumber")
            .expect("phone arm");
        let end = auth[start..]
            .find("AuthorizationState::WaitCode")
            .expect("code arm");
        let phone = &auth[start..start + end];
        assert!(phone.contains("set_secret(TelegramSecretKey::Session, \"\")"));
        assert!(phone.contains("request_secret_flush"));
        assert!(phone.contains("TelegramAuthPhase::NeedPhone"));
    }

    fn fn_body<'a>(src: &'a str, name: &str) -> &'a str {
        let start = src.find(name).unwrap_or_else(|| panic!("{name} missing"));
        let rest = &src[start..];
        let end = rest[name.len()..]
            .find("\nfn ")
            .or_else(|| rest[name.len()..].find("\nasync fn "))
            .map(|offset| offset + name.len())
            .unwrap_or(rest.len());
        &rest[..end]
    }

    #[test]
    fn tdlib_generate_db_key_does_not_use_weak_entropy() {
        let src = include_str!("tdlib.rs");
        let start = src
            .find("fn generate_db_key()")
            .expect("generate_db_key must stay in tdlib.rs");
        let rest = &src[start..];
        let end = rest[1..]
            .find("\nfn ")
            .map(|offset| offset + 1)
            .unwrap_or(rest.len());
        let body = &rest[..end];
        assert!(
            body.contains("super::db_key::generate_db_key") || body.contains("getrandom"),
            "generate_db_key must use the CSPRNG path"
        );
        assert!(!body.contains("DefaultHasher"));
        assert!(!body.contains("SystemTime"));
        assert!(!body.contains("UNIX_EPOCH"));
        assert!(!src.contains("process::id"));
        assert!(!include_str!("db_key.rs").contains("DefaultHasher"));
    }

    #[test]
    fn send_or_respawn_sends_on_a_live_channel() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut slot = Some(tx);
        let mut spawned = 0;
        assert!(send_or_respawn(&mut slot, 7u8, || {
            spawned += 1;
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            tx
        }));
        assert_eq!(spawned, 0);
        assert_eq!(rx.try_recv().expect("message"), 7);
        assert!(slot.is_some());
    }

    #[test]
    fn send_or_respawn_spawns_when_slot_is_empty() {
        let mut slot = None;
        let mut spawned = 0;
        let (hold_tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(send_or_respawn(&mut slot, 3u8, || {
            spawned += 1;
            hold_tx.clone()
        }));
        assert_eq!(spawned, 1);
        assert_eq!(rx.try_recv().expect("message"), 3);
        assert!(slot.is_some());
    }

    #[test]
    fn send_or_respawn_clears_dead_sender_and_retries_once() {
        let (dead_tx, dead_rx) = tokio::sync::mpsc::unbounded_channel();
        drop(dead_rx);
        let mut slot = Some(dead_tx);
        let mut spawned = 0;
        let (live_tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(send_or_respawn(&mut slot, 9u8, || {
            spawned += 1;
            live_tx.clone()
        }));
        assert_eq!(spawned, 1);
        assert_eq!(rx.try_recv().expect("retried message"), 9);
        assert!(slot.is_some());
    }

    #[test]
    fn send_or_respawn_fails_only_after_retry_dies() {
        let (dead_tx, dead_rx) = tokio::sync::mpsc::unbounded_channel();
        drop(dead_rx);
        let mut slot = Some(dead_tx);
        let mut spawned = 0;
        assert!(!send_or_respawn(&mut slot, 1u8, || {
            spawned += 1;
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            drop(rx);
            tx
        }));
        assert_eq!(spawned, 1);
        assert!(slot.is_none());
    }

    #[test]
    fn request_secret_flush_event_carries_no_values() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        request_secret_flush(&tx);
        let event = rx.try_recv().expect("flush");
        assert_eq!(event, AdapterEvent::FlushSecrets);
        let debug = format!("{event:?}");
        assert!(debug.contains("FlushSecrets"));
        assert!(!debug.contains("api_hash"));
        assert!(!debug.contains("db_key"));
    }

    #[test]
    fn disconnect_resets_engine_and_does_not_emit_ready() {
        let vault = Arc::new(MemorySecretVault::new());
        vault.set_secret(TelegramSecretKey::ApiId, "11111");
        vault.set_secret(TelegramSecretKey::ApiHash, "hash-value");
        let mut adapter = TelegramAdapter::new(Arc::clone(&vault) as Arc<dyn TelegramSecretVault>);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        adapter
            .handle(
                AdapterCommand::TelegramAuth {
                    step: TelegramAuthStep::ApiCredentials,
                },
                &tx,
            )
            .expect("auth");
        adapter
            .handle(
                AdapterCommand::Disconnect {
                    protocol: ProtocolId::Telegram,
                },
                &tx,
            )
            .expect("disconnect");
        assert_eq!(adapter.engine.last_phase(), None);
        let mut saw_ready = false;
        while let Ok(event) = rx.try_recv() {
            if let AdapterEvent::TelegramAuth {
                phase: TelegramAuthPhase::Ready,
            } = event
            {
                saw_ready = true;
            }
            let debug = format!("{event:?}");
            assert!(!debug.contains("11111"));
            assert!(!debug.contains("hash-value"));
        }
        assert!(!saw_ready);
    }

    #[test]
    fn inbox_commands_stay_off_the_ui_and_do_not_carry_secrets() {
        let vault = Arc::new(MemorySecretVault::new());
        vault.set_secret(TelegramSecretKey::ApiId, "11111");
        vault.set_secret(TelegramSecretKey::ApiHash, "hash-value");
        let mut adapter = TelegramAdapter::new(Arc::clone(&vault) as Arc<dyn TelegramSecretVault>);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let open = adapter.handle(
            AdapterCommand::OpenChat {
                protocol: ProtocolId::Telegram,
                conversation_id: "telegram:saved".into(),
            },
            &tx,
        );
        assert!(
            open.expect_err("placeholder")
                .to_string()
                .contains("not a Telegram chat")
        );
        let empty = adapter.handle(
            AdapterCommand::SendText {
                protocol: ProtocolId::Telegram,
                conversation_id: "telegram:42".into(),
                body: "   ".into(),
            },
            &tx,
        );
        assert!(empty.expect_err("blank").to_string().contains("empty"));
        let send = adapter.handle(
            AdapterCommand::SendText {
                protocol: ProtocolId::Telegram,
                conversation_id: "telegram:42".into(),
                body: "hello".into(),
            },
            &tx,
        );
        let load = adapter.handle(
            AdapterCommand::LoadChats {
                protocol: ProtocolId::Telegram,
            },
            &tx,
        );
        if uses_tdlib_hook() {
            send.expect("live send queues on the worker");
            load.expect("live load queues on the worker");
        } else {
            assert!(
                send.expect_err("feature off")
                    .to_string()
                    .contains("TDLib unavailable")
            );
            assert!(
                load.expect_err("feature off")
                    .to_string()
                    .contains("TDLib unavailable")
            );
        }
        while let Ok(event) = rx.try_recv() {
            let debug = format!("{event:?}");
            assert!(!debug.contains("11111"), "{debug}");
            assert!(!debug.contains("hash-value"), "{debug}");
        }
        let command = AdapterCommand::SendText {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:42".into(),
            body: "hello".into(),
        };
        let debug = format!("{command:?}");
        assert!(debug.contains("hello"));
        assert!(!debug.contains("11111"));
        assert!(!debug.contains("hash-value"));
    }

    #[test]
    fn nonnumeric_api_id_emits_failed_phase() {
        let vault = Arc::new(MemorySecretVault::new());
        vault.set_secret(TelegramSecretKey::ApiId, "not-a-number");
        vault.set_secret(TelegramSecretKey::ApiHash, "hash-value");
        let mut adapter = TelegramAdapter::new(Arc::clone(&vault) as Arc<dyn TelegramSecretVault>);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let err = adapter
            .handle(
                AdapterCommand::TelegramAuth {
                    step: TelegramAuthStep::ApiCredentials,
                },
                &tx,
            )
            .expect_err("nonnumeric");
        assert!(err.to_string().contains("must be a number"));
        assert!(!err.to_string().contains("not-a-number"));
        let event = rx.try_recv().expect("failed phase");
        match event {
            AdapterEvent::TelegramAuth {
                phase: TelegramAuthPhase::Failed,
            } => {}
            other => panic!("expected Failed, got {other:?}"),
        }
    }
}
