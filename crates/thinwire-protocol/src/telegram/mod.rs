//! Telegram adapter on the official TDLib path (`tdlib-rs`).
//!
//! Default builds keep a compile-safe auth state machine so CI does not need
//! system TDLib. Enable `telegram-tdlib` to compile the live client. Read
//! credentials from [`TelegramSecretVault`] — never put them on commands, never
//! log them, never commit them.

mod auth_error;
mod credentials;
mod data_dir;
mod db_key;
mod engine;
mod inbox;
mod lifecycle;
mod router;

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

#[cfg(test)]
use super::adapter::TelegramCodeVia;
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
            AdapterCommand::Shutdown {
                protocol: ProtocolId::Telegram,
            } => {
                self.engine.reset();
                #[cfg(feature = "telegram-tdlib")]
                self.tdlib.shutdown(events);
                #[cfg(not(feature = "telegram-tdlib"))]
                super::adapter::emit_stopped(events, ProtocolId::Telegram);
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
            AdapterCommand::ResendMessage {
                protocol: ProtocolId::Telegram,
                conversation_id,
                message_id,
            } => {
                let owned = inbox::parse_message_id(&message_id)
                    .is_some_and(|(chat_id, _)| inbox::conversation_id(chat_id) == conversation_id);
                if !owned {
                    return Err(AdapterError::Unavailable {
                        protocol: ProtocolId::Telegram,
                        reason: "message id is not in this Telegram chat",
                    });
                }
                self.dispatch_live(
                    events,
                    LiveCall::Resend {
                        conversation_id,
                        message_id,
                    },
                )
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
    Resend {
        conversation_id: String,
        message_id: String,
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
                LiveCall::Resend {
                    conversation_id,
                    message_id,
                } => {
                    self.tdlib
                        .resend(conversation_id, message_id, secrets, source, events);
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

    // A runtime for the live build's `tokio::spawn` (qa R44). The body never
    // awaits, so the worker is never polled and no TDLib client starts.
    #[tokio::test]
    async fn telegram_auth_step_does_not_echo_secrets() {
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
        assert!(updates.contains("set_preview(update.chat_id, \"\", 0)"));
        let open = fn_body(src, "async fn open_chat");
        assert!(open.contains("load_history"));
        assert!(open.contains("emit_history_loaded"));
        let history = fn_body(src, "async fn load_history");
        assert!(history.contains("functions::view_messages"));
        assert!(history.contains("true,"));
        let chats = fn_body(src, "async fn load_main_chats");
        assert!(chats.contains("emit_chat_list_loaded"));
    }

    // A runtime for the live build's `tokio::spawn` (qa R44). The body never
    // awaits, so the worker is never polled and no TDLib client starts.
    #[tokio::test]
    async fn resend_checks_the_chat_and_uses_tdlib_resend() {
        let mut adapter = TelegramAdapter::memory();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let foreign = adapter.handle(
            AdapterCommand::ResendMessage {
                protocol: ProtocolId::Telegram,
                conversation_id: "telegram:1".into(),
                message_id: "telegram:2:5".into(),
            },
            &tx,
        );
        assert!(
            foreign
                .expect_err("other chat")
                .to_string()
                .contains("not in this Telegram chat")
        );
        let resend = adapter.handle(
            AdapterCommand::ResendMessage {
                protocol: ProtocolId::Telegram,
                conversation_id: "telegram:1".into(),
                message_id: "telegram:1:5".into(),
            },
            &tx,
        );
        if uses_tdlib_hook() {
            resend.expect("live resend queues on the worker");
        } else {
            assert!(
                resend
                    .expect_err("feature off")
                    .to_string()
                    .contains("TDLib unavailable")
            );
        }
        let src = include_str!("tdlib.rs");
        let body = fn_body(src, "async fn resend");
        assert!(body.contains("functions::resend_messages"));
        assert!(body.contains("Delivery::Failed"));
        let mapped = fn_body(src, "fn emit_mapped_message");
        assert!(mapped.contains("MessageSendingState::Pending"));
        assert!(mapped.contains("MessageSendingState::Failed"));
    }

    #[test]
    fn live_tdlib_reports_why_a_login_step_failed_and_where_the_code_went() {
        let src = include_str!("tdlib.rs");
        let steps = fn_body(src, "async fn apply_step");
        assert_eq!(steps.matches("reject_step(events, &error)").count(), 3);
        let reject = fn_body(src, "fn reject_step");
        assert!(reject.contains("auth_error_from_tdlib(error.code, &error.message)"));
        assert!(reject.contains("emit_telegram_auth_rejected"));
        assert!(reject.contains("TelegramAuthPhase::Failed"));
        let auth = fn_body(src, "async fn apply_authorization");
        assert!(auth.contains("emit_telegram_code_sent(events, code_via("));
        let via = fn_body(src, "fn code_via");
        assert!(via.contains("Kind::SmsWord(_) | Kind::SmsPhrase(_) => TelegramCodeVia::SmsWord"));
        assert!(TelegramCodeVia::Sms.digits_only());
        assert!(!TelegramCodeVia::SmsWord.digits_only(), "qa R14");
        assert!(!auth.contains("optional 2FA"));
        assert!(!src.contains("TDLib worker is not running"));
    }

    #[test]
    fn live_tdlib_maps_message_dates_and_group_chats() {
        let src = include_str!("tdlib.rs");
        let mapped = fn_body(src, "fn emit_mapped_message");
        assert!(mapped.contains("sent_at: i64::from(message.date)"));
        let chat = fn_body(src, "fn note_chat");
        assert!(chat.contains("i64::from(message.date)"));
        assert!(chat.contains("ChatType::BasicGroup(_) => true"));
        assert!(chat.contains("!group.is_channel"));
        let updates = fn_body(src, "fn apply_chat_update");
        assert!(updates.contains("i64::from(message.date)"));
        assert!(updates.contains("i64::from(update.message.date)"));
    }

    #[test]
    fn shutdown_reports_stopped_and_workers_close_tdlib_first() {
        let mut adapter = TelegramAdapter::memory();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        if !uses_tdlib_hook() {
            adapter
                .handle(
                    AdapterCommand::Shutdown {
                        protocol: ProtocolId::Telegram,
                    },
                    &tx,
                )
                .expect("shutdown");
            assert_eq!(
                rx.try_recv().expect("stopped"),
                AdapterEvent::Stopped {
                    protocol: ProtocolId::Telegram
                }
            );
        }
        let src = include_str!("tdlib.rs");
        let stop = &src[src.find("pub fn stop(").expect("stop")
            ..src.find("pub fn shutdown(").expect("shutdown")];
        assert!(
            stop.contains("TdlibCommand::Close"),
            "stop closes, it does not drop"
        );
        let shutdown = fn_body(src, "pub fn shutdown(");
        assert!(shutdown.contains("all_done(&workers)"));
        assert!(shutdown.contains("emit_stopped"));
        assert!(
            shutdown.contains("receiver_idle()"),
            "exit waits until no thread is in TDLib"
        );
        let worker = fn_body(src, "fn spawn_tdlib_worker");
        assert!(worker.contains("request_close(client_id)"));
        assert!(worker.contains("if closed"));
        let register = worker
            .find("dispatcher.register(client_id)")
            .expect("register");
        let first_request = worker
            .find("set_log_verbosity_level")
            .expect("first request");
        assert!(
            register < first_request,
            "register before the first request"
        );
        let unregister = worker
            .find("dispatcher.unregister(client_id)")
            .expect("unregister");
        let done = worker.rfind("mark_done(&done)").expect("done");
        assert!(unregister < done);
        assert!(fn_body(src, "async fn request_close").contains("functions::close"));
        assert!(
            !src.contains("generation"),
            "no worker is dropped without close"
        );
    }

    #[test]
    fn a_new_client_opens_only_after_the_old_one_released_the_database() {
        let src = include_str!("tdlib.rs");
        let stop = &src[src.find("pub fn stop(").expect("stop")
            ..src.find("pub fn shutdown(").expect("shutdown")];
        assert!(stop.contains("retire_current()"));
        let enqueue = fn_body(src, "fn enqueue(");
        assert!(enqueue.contains("slots.start()"));
        let guard = enqueue
            .find("self.slots.is_shut()")
            .expect("shutdown guard");
        let spawn = enqueue.find("send_or_respawn").expect("spawn");
        assert!(guard < spawn, "no new client after Shutdown (qa R50)");
        let worker = fn_body(src, "fn spawn_tdlib_worker");
        let wait = worker.find("wait_for_retired(&wait_for)").expect("wait");
        let create = worker.find("create_client()").expect("create");
        assert!(
            wait < create,
            "wait before the new client opens the database"
        );
        let timeout = fn_body(src, "async fn wait_for_retired");
        assert!(timeout.contains("RETIRE_TIMEOUT"));
    }

    #[test]
    fn failed_tdlib_parameters_stop_the_login_and_log_safely() {
        let src = include_str!("tdlib.rs");
        let params = fn_body(src, "async fn set_parameters");
        assert!(params.contains("if let Err(error) = result"));
        assert!(fn_body(src, "async fn send_parameters").contains("set_tdlib_parameters("));
        assert!(params.contains("TelegramAuthError::ClientSetup"));
        assert!(params.contains("TelegramAuthPhase::Failed"));
        assert!(params.contains("log_tdlib_error(\"setTdlibParameters\""));
        let log = fn_body(src, "fn log_tdlib_error");
        assert!(log.contains("loggable_tdlib_message"));
    }

    #[test]
    fn live_tdlib_moves_a_keyless_data_folder_aside_before_it_opens() {
        let src = include_str!("tdlib.rs");
        let params = fn_body(src, "async fn set_parameters");
        let check = params
            .find("move_aside_if_keyless(&dir, has_key)")
            .expect("check");
        let key = params.find("ensure_db_key(").expect("key");
        assert!(check < key, "check the vault before a new key is made");
        assert!(params.contains("data_dir::is_wrong_key_error(&error.message)"));
        assert!(params.contains("&& moved_to.is_none()"), "retry once only");
        assert!(params.contains("emit_telegram_data_reset(events, name)"));
        assert!(
            !src.contains("remove_dir_all"),
            "old data is moved, never deleted"
        );
    }

    #[test]
    fn live_tdlib_uses_a_throwaway_folder_without_a_saving_keychain() {
        let src = include_str!("tdlib.rs");
        let params = fn_body(src, "async fn set_parameters");
        assert!(params.contains("tdlib_data_dir(secrets.persists())"));
        let dir = fn_body(src, "fn tdlib_data_dir");
        assert!(dir.contains("data_dir::this_process_session_dir()"));
        let shutdown = &src[src.find("pub fn shutdown(").expect("shutdown")..];
        let shutdown = &shutdown[..shutdown.find("\n    }\n").expect("end")];
        let idle = shutdown.find("receiver_idle()").expect("idle");
        let remove = shutdown
            .find("data_dir::remove_this_process_session_dir()")
            .expect("throwaway folder is removed at a clean exit");
        assert!(idle < remove, "remove only after no thread is in TDLib");
        assert!(
            !TelegramSecretVault::persists(&MemorySecretVault::new()),
            "tests never open the real TDLib folder (qa R45)"
        );
    }

    #[test]
    fn every_tdlib_error_goes_through_the_safe_log() {
        let src = include_str!("tdlib.rs");
        for request in [
            "\"sendMessage (update)\"",
            "\"close\"",
            "\"login step\"",
            "\"loadChats\"",
            "\"viewMessages\"",
            "\"getChatHistory\"",
            "\"sendMessage\"",
            "\"resendMessages\"",
            "\"setTdlibParameters\"",
        ] {
            assert!(
                src.contains(&format!("log_tdlib_error({request}")),
                "{request}"
            );
        }
        assert_eq!(
            fn_body(src, "fn reject_step")
                .matches("log_tdlib_error(")
                .count(),
            1,
            "the three login arms log through reject_step"
        );
        let log = fn_body(src, "fn log_tdlib_error");
        assert!(log.contains("loggable_tdlib_message(&error.message)"));
        assert!(!log.contains("error.message,"), "raw text is never logged");
    }

    #[test]
    fn data_reset_event_names_the_folder_never_a_path() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        crate::adapter::emit_telegram_data_reset(
            &tx,
            "/home/user/.local/share/thinwire/tdlib.stale-1790000000",
        );
        assert_eq!(
            rx.try_recv().expect("event"),
            AdapterEvent::TelegramDataReset {
                moved_to: "tdlib.stale-1790000000".into()
            }
        );
    }

    #[test]
    fn live_tdlib_reports_a_remote_logout_but_not_its_own_close() {
        let src = include_str!("tdlib.rs");
        let auth = fn_body(src, "async fn apply_authorization");
        let arm = &auth[auth.find("AuthorizationState::LoggingOut").expect("arm")..];
        assert!(arm.contains("was_authorized && !live.closing"));
        assert!(arm.contains("emit_telegram_session_ended(events)"));
        let worker = fn_body(src, "fn spawn_tdlib_worker");
        assert!(
            worker.contains("live.closing = true;"),
            "our Close is not a logout"
        );
    }

    #[test]
    fn live_tdlib_has_one_receive_thread_for_the_process() {
        let src = include_str!("tdlib.rs");
        assert_eq!(src.matches("tdlib_rs::receive()").count(), 1);
        assert_eq!(src.matches(".name(\"thinwire-tdlib-recv\"").count(), 1);
        assert!(src.contains("static DISPATCHER: OnceLock"));
        let receive = fn_body(src, "fn receive_loop");
        assert!(receive.contains("begin_receive()"));
        assert!(receive.contains("end_receive()"));
        assert!(receive.contains("route(client_id, update)"));
        let worker = fn_body(src, "fn spawn_tdlib_worker");
        assert!(
            !worker.contains("thread::Builder"),
            "workers do not start receive threads"
        );
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

    // A runtime for the live build's `tokio::spawn` (qa R44). The body never
    // awaits, so the worker is never polled and no TDLib client starts.
    #[tokio::test]
    async fn disconnect_resets_engine_and_does_not_emit_ready() {
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

    // A runtime for the live build's `tokio::spawn` (qa R44). The body never
    // awaits, so the worker is never polled and no TDLib client starts.
    #[tokio::test]
    async fn inbox_commands_stay_off_the_ui_and_do_not_carry_secrets() {
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
