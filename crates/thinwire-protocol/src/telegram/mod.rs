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
    AdapterCommand, AdapterError, AdapterEvent, AdapterStatus, EventTx, ProtocolAdapter,
    ProtocolCapabilities, ProtocolId, SupportClass, TelegramAuthPhase, TelegramAuthStep,
    emit_flush_secrets, emit_older_history_loaded, emit_send_rejected, emit_status,
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
    /// Host login epoch. A step sent under an older value is ignored.
    login_epoch: super::adapter::LoginEpoch,
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
        Self::with_login_epoch(secrets, api_source, std::sync::Arc::default())
    }

    /// The host's adapter: it shares the host's login epoch, so a step queued
    /// before Cancel is ignored, and login events of that client are dropped.
    #[must_use]
    pub(crate) fn with_login_epoch(
        secrets: Arc<dyn TelegramSecretVault>,
        api_source: TelegramApiSource,
        login_epoch: super::adapter::LoginEpoch,
    ) -> Self {
        Self {
            secrets,
            api_source,
            engine: TelegramAuthEngine::new(),
            login_epoch: std::sync::Arc::clone(&login_epoch),
            #[cfg(feature = "telegram-tdlib")]
            tdlib: tdlib::TdlibRuntime::with_login_epoch(login_epoch),
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
        epoch: u64,
        events: &EventTx,
    ) -> Result<(), AdapterError> {
        // Cancel bumps the epoch and clears the secrets before this command
        // is dequeued. Running it would emit an unstamped `Failed` (the phone
        // or code is already gone) onto the cancelled screen or the next login.
        if epoch != self.login_epoch.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(());
        }
        let phase = match self
            .engine
            .submit(step, self.secrets.as_ref(), &self.api_source)
        {
            Ok(phase) => phase,
            Err(error) => {
                // Cancel can clear the phone or code between the check above
                // and here. Stamp the failure with the step's epoch: the host
                // drops it at delivery if Cancel moved the epoch (PR #49 review).
                let _ = events.send(AdapterEvent::Login {
                    epoch,
                    event: Box::new(AdapterEvent::TelegramAuth {
                        phase: TelegramAuthPhase::Failed,
                    }),
                });
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
            let _ = events.send(AdapterEvent::Login {
                epoch,
                event: Box::new(AdapterEvent::TelegramAuth { phase }),
            });
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

    /// Close every TDLib client, then `Stopped` (no TDLib: `Stopped` at once).
    fn shutdown(&mut self, events: &EventTx) {
        self.engine.reset();
        #[cfg(feature = "telegram-tdlib")]
        self.tdlib.shutdown(events);
        #[cfg(not(feature = "telegram-tdlib"))]
        super::adapter::emit_stopped(events, ProtocolId::Telegram);
    }

    fn handle(&mut self, command: AdapterCommand, events: &EventTx) -> Result<(), AdapterError> {
        match command {
            AdapterCommand::TelegramAuth { step, epoch } => self.handle_auth(step, epoch, events),
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
            AdapterCommand::LoadOlderMessages {
                protocol: ProtocolId::Telegram,
                conversation_id,
                before_message_id,
            } => {
                let owned = inbox::parse_message_id(&before_message_id)
                    .is_some_and(|(chat_id, _)| inbox::conversation_id(chat_id) == conversation_id);
                let result = if owned {
                    self.dispatch_live(
                        events,
                        LiveCall::LoadOlder {
                            conversation_id: conversation_id.clone(),
                            before_message_id: before_message_id.clone(),
                        },
                    )
                } else {
                    Err(AdapterError::Unavailable {
                        protocol: ProtocolId::Telegram,
                        reason: "message id is not in this Telegram chat",
                    })
                };
                // A refused request ends too, so the UI stops its spinner and
                // does not ask again in this state.
                if result.is_err() {
                    emit_older_history_loaded(
                        events,
                        ProtocolId::Telegram,
                        conversation_id,
                        before_message_id,
                        false,
                        None,
                    );
                }
                result
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
                request,
            } => {
                let rejected = if body.trim().is_empty() {
                    Some("message text is empty")
                } else if parse_telegram_chat_id(&conversation_id).is_none() {
                    Some("chat id is not a Telegram chat")
                } else {
                    None
                };
                let result = match rejected {
                    Some(reason) => Err(AdapterError::Unavailable {
                        protocol: ProtocolId::Telegram,
                        reason,
                    }),
                    None => self.dispatch_live(
                        events,
                        LiveCall::SendText {
                            conversation_id: conversation_id.clone(),
                            body,
                            request,
                        },
                    ),
                };
                // Name this send in the rejection, so the UI fails only it.
                if result.is_err() {
                    emit_send_rejected(events, ProtocolId::Telegram, conversation_id, request);
                }
                result
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
        request: u64,
    },
    Resend {
        conversation_id: String,
        message_id: String,
    },
    LoadOlder {
        conversation_id: String,
        before_message_id: String,
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
                    request,
                } => {
                    self.tdlib
                        .send_text(conversation_id, body, request, secrets, source, events);
                }
                LiveCall::LoadOlder {
                    conversation_id,
                    before_message_id,
                } => {
                    self.tdlib.load_older(
                        conversation_id,
                        before_message_id,
                        secrets,
                        source,
                        events,
                    );
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

    /// A login phase carries its step's epoch; the host unwraps it.
    fn unstamp(event: AdapterEvent) -> AdapterEvent {
        match event {
            AdapterEvent::Login { event, .. } => *event,
            other => other,
        }
    }

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
                    epoch: 0,
                },
                &tx,
            )
            .expect("auth step");
        let mut saw_phase = false;
        while let Ok(event) = rx.try_recv() {
            match unstamp(event) {
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
                step: TelegramAuthStep::ApiCredentials,
                epoch: 0
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
                .handle(AdapterCommand::TelegramAuth { step, epoch: 0 }, &tx)
                .expect("step");
            let event = unstamp(rx.try_recv().expect("phase event"));
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
        assert_eq!(
            steps.matches("reject_step(&login, &error)").count(),
            4,
            "phone, code, resend code, password"
        );
        let reject = fn_body(src, "fn reject_step");
        assert!(reject.contains("auth_error_from_tdlib(error.code, &error.message)"));
        assert!(reject.contains("login.rejected(reason)"));
        assert!(reject.contains("TelegramAuthPhase::Failed"));
        let auth = fn_body(src, "async fn apply_authorization");
        assert!(auth.contains("login.code_sent(code_via("));
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
            adapter.shutdown(&tx);
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
            shutdown.contains("SHUTDOWN_LIMIT"),
            "shutdown has its own bound"
        );
        assert!(
            src.contains("const SHUTDOWN_LIMIT: Duration = Duration::from_secs(4);"),
            "below the app's 5 s close limit"
        );
        assert!(
            fn_body(src, "fn receiver_idle").contains("dispatcher.running.load"),
            "a thread that never started counts as idle"
        );
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
        let settled = params
            .find("secrets.secrets_hydrated()")
            .expect("settled read");
        let check = params
            .find("move_aside_if_keyless(&dir, has_key)")
            .expect("check");
        let key = params.find("ensure_db_key(").expect("key");
        assert!(
            settled < check,
            "a failed hydrate must not look like a missing key"
        );
        assert!(check < key, "check the vault before a new key is made");
        assert!(params.contains("data_dir::is_wrong_key_error(error.code, &error.message)"));
        assert!(params.contains("&& moved_to.is_none()"), "retry once only");
        assert!(params.contains("login.data_reset(name)"));
        assert!(
            !src.contains("remove_dir_all"),
            "old data is moved, never deleted"
        );
    }

    #[test]
    fn live_tdlib_uses_a_throwaway_folder_without_a_saving_keychain() {
        let src = include_str!("tdlib.rs");
        let params = fn_body(src, "async fn set_parameters");
        assert!(params.contains("tdlib_data_dir(secrets.persists(), secrets.tdlib_folder_name())"));
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
        assert!(arm.contains("live.ended_elsewhere = true"));
        assert!(
            !auth.contains("emit_telegram_session_ended"),
            "not before Closed (qa R74)"
        );
        let worker = fn_body(src, "fn spawn_tdlib_worker");
        assert!(
            worker.contains("live.closing.mark();"),
            "our Close is not a logout"
        );
        let dropped = worker.find("drop(commands);").expect("drop");
        let done = worker.rfind("mark_done(&done)").expect("done");
        let event = worker
            .find("emit_telegram_session_ended(&events)")
            .expect("event after the loop");
        assert!(
            dropped < done && done < event,
            "the next command must start a new client"
        );
    }

    #[test]
    fn resend_code_uses_tdlib_resend_and_the_stub_stays_on_the_code_step() {
        let src = include_str!("tdlib.rs");
        let steps = fn_body(src, "async fn apply_step");
        let arm = &steps[steps.find("TelegramAuthStep::ResendCode").expect("arm")..];
        let arm = &arm[..arm.find("TelegramAuthStep::Code").expect("next arm")];
        assert!(arm.contains("functions::resend_authentication_code("));
        assert!(arm.contains("ResendCodeReason::UserRequest"));
        assert!(arm.contains("reject_step(&login, &error)"));
        assert!(!arm.contains("set_authentication_phone_number"));
        if uses_tdlib_hook() {
            return;
        }
        let vault = Arc::new(MemorySecretVault::new());
        vault.set_secret(TelegramSecretKey::ApiId, "11111");
        vault.set_secret(TelegramSecretKey::ApiHash, "hash-value");
        let mut adapter = TelegramAdapter::new(Arc::clone(&vault) as Arc<dyn TelegramSecretVault>);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        adapter
            .handle(
                AdapterCommand::TelegramAuth {
                    step: TelegramAuthStep::ResendCode,
                    epoch: 0,
                },
                &tx,
            )
            .expect("resend");
        assert_eq!(
            rx.try_recv().expect("phase"),
            AdapterEvent::Login {
                epoch: 0,
                event: Box::new(AdapterEvent::TelegramAuth {
                    phase: TelegramAuthPhase::NeedCode
                }),
            }
        );
    }

    #[test]
    fn live_tdlib_drops_a_chat_that_left_the_main_list() {
        let src = include_str!("tdlib.rs");
        let updates = fn_body(src, "fn apply_chat_update");
        let arm = &updates[updates.find("Update::ChatLastMessage").expect("arm")..];
        let arm = &arm[..arm.find("Update::NewMessage").expect("next arm")];
        assert!(arm.contains("set_main_position(update.chat_id, main_order(&update.positions))"));
        assert!(
            !arm.contains("if let Some(order) = main_order"),
            "None must not be ignored"
        );
    }

    #[test]
    fn a_refused_send_names_its_chat_and_request() {
        let mut adapter = TelegramAdapter::memory();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let _ = adapter.handle(
            AdapterCommand::SendText {
                protocol: ProtocolId::Telegram,
                conversation_id: "telegram:42".into(),
                body: "   ".into(),
                request: 7,
            },
            &tx,
        );
        let mut rejected = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let AdapterEvent::SendRejected {
                conversation_id,
                request,
                ..
            } = event
            {
                rejected.push((conversation_id, request));
            }
        }
        assert_eq!(rejected, vec![("telegram:42".to_string(), 7)]);
        let src = include_str!("tdlib.rs");
        let send = fn_body(src, "async fn send_text");
        assert_eq!(
            send.matches("rejected();").count(),
            4,
            "not ready, bad chat id, empty text, and TDLib error each name the send"
        );
        let pending = send
            .find("emit_mapped_message(events, &message")
            .expect("pending row");
        let accepted = send
            .find("emit_send_accepted(events, ProtocolId::Telegram, conversation_id, request)")
            .expect("acceptance names the send");
        assert!(pending < accepted, "the pending row comes first");
    }

    #[test]
    fn live_tdlib_takes_the_data_folder_name_from_the_vault() {
        let src = include_str!("tdlib.rs");
        let params = fn_body(src, "async fn set_parameters");
        assert!(params.contains("tdlib_data_dir(secrets.persists(), secrets.tdlib_folder_name())"));
        let dir = fn_body(src, "fn tdlib_data_dir");
        assert!(dir.contains("base.push(folder_name)"));
        assert!(!dir.contains("base.push(\"tdlib\")"));
        assert_eq!(
            TelegramSecretVault::tdlib_folder_name(&MemorySecretVault::new()),
            crate::secrets::TDLIB_FOLDER
        );
    }

    // A runtime for the live build's `tokio::spawn`. The body never awaits,
    // so the worker is never polled and no TDLib client starts.
    #[tokio::test]
    async fn older_history_requests_are_checked_and_always_end() {
        let mut adapter = TelegramAdapter::memory();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let foreign = adapter.handle(
            AdapterCommand::LoadOlderMessages {
                protocol: ProtocolId::Telegram,
                conversation_id: "telegram:1".into(),
                before_message_id: "telegram:2:50".into(),
            },
            &tx,
        );
        assert!(
            foreign
                .expect_err("other chat")
                .to_string()
                .contains("not in this Telegram chat")
        );
        assert_eq!(
            rx.try_recv().expect("the request ends"),
            AdapterEvent::OlderHistoryLoaded {
                protocol: ProtocolId::Telegram,
                conversation_id: "telegram:1".into(),
                before_message_id: "telegram:2:50".into(),
                more: false,
                note: None,
            }
        );
        let valid = adapter.handle(
            AdapterCommand::LoadOlderMessages {
                protocol: ProtocolId::Telegram,
                conversation_id: "telegram:1".into(),
                before_message_id: "telegram:1:50".into(),
            },
            &tx,
        );
        if uses_tdlib_hook() {
            valid.expect("queued on the worker");
        } else {
            assert!(
                valid
                    .expect_err("feature off")
                    .to_string()
                    .contains("TDLib unavailable")
            );
            assert!(matches!(
                rx.try_recv().expect("ends without TDLib"),
                AdapterEvent::OlderHistoryLoaded { more: false, .. }
            ));
        }
        let debug = format!(
            "{:?}",
            AdapterCommand::LoadOlderMessages {
                protocol: ProtocolId::Telegram,
                conversation_id: "telegram:1".into(),
                before_message_id: "telegram:1:50".into(),
            }
        );
        assert!(debug.contains("telegram:1:50"), "ids only, no secret");
    }

    #[test]
    fn live_tdlib_loads_older_pages_off_the_ui_thread_without_logging_text() {
        let src = include_str!("tdlib.rs");
        // Bounded by hand: `fn_body` looks for the next sync `fn` first.
        let older = &src[src.find("async fn load_older").expect("load_older")
            ..src.find("async fn send_text(").expect("next fn")];
        assert!(older.contains("live.older.begin(chat_id, before)"));
        assert!(older.contains("fetch_older_page(client_id, chat_id, before)"));
        assert!(
            older.contains("PageOutcome::OnlyAnchor"),
            "an anchor-only page is asked once more, not the start"
        );
        assert!(older.contains("live.older.finish(chat_id, before, outcome)"));
        let fetch = &src[src.find("async fn fetch_older_page").expect("fetch")
            ..src.find("async fn send_text(").expect("next fn")];
        assert!(fetch.contains("inbox::OLDER_PAGE_LIMIT"));
        assert!(fetch.contains("functions::get_chat_history("));
        assert!(fetch.contains("inbox::page_outcome(raw_len, older.len())"));
        assert!(older.contains("log_tdlib_error(\"getChatHistory (older)\""));
        assert!(!older.contains("tracing::"), "no message text in logs");
        assert!(
            older.matches("done(").count() + older.matches("end(more, note)").count() >= 5,
            "every path ends the request"
        );
        let worker = fn_body(src, "fn spawn_tdlib_worker");
        assert!(
            worker.contains("TdlibCommand::LoadOlder"),
            "runs on the worker, one command at a time"
        );
    }

    #[test]
    fn a_closing_client_sends_no_login_updates_after_cancel() {
        let src = include_str!("tdlib.rs");
        let stop = &src[src.find("pub fn stop(").expect("stop")
            ..src.find("pub fn shutdown(").expect("shutdown")];
        let mark = stop.find("closing.mark()").expect("stop marks the client");
        let close = stop.find("TdlibCommand::Close").expect("then queues Close");
        assert!(
            mark < close,
            "no login event after Cancel, even before Close is read"
        );
        let auth = fn_body(src, "async fn apply_authorization");
        assert!(
            !auth.contains("emit_telegram_auth("),
            "every login event is gated"
        );
        let ready = &auth[auth.find("AuthorizationState::Ready").expect("ready")..];
        let guard = ready.find("if !login.open()").expect("ready guard");
        let marker = ready.find("TDLIB_SESSION_MARKER").expect("marker");
        assert!(
            guard < marker,
            "a closing client does not link or save a session"
        );
        for body in [
            "async fn apply_step",
            "fn reject_step",
            "async fn set_parameters",
        ] {
            let body = fn_body(src, body);
            assert!(!body.contains("emit_telegram_auth("), "{body}");
            assert!(!body.contains("emit_telegram_auth_rejected("));
        }
    }

    #[test]
    fn a_step_that_fails_after_cancel_cleared_its_secret_is_dropped_at_delivery() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let vault = Arc::new(MemorySecretVault::new());
        vault.set_secret(TelegramSecretKey::ApiId, "11111");
        vault.set_secret(TelegramSecretKey::ApiHash, "hash-value");
        let epoch = Arc::new(AtomicU64::new(0));
        let mut adapter = TelegramAdapter::with_login_epoch(
            Arc::clone(&vault) as Arc<dyn TelegramSecretVault>,
            TelegramApiSource::from_build(),
            Arc::clone(&epoch),
        );
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        // The step passed the epoch check; then Cancel cleared the phone.
        let result = adapter.handle(
            AdapterCommand::TelegramAuth {
                step: TelegramAuthStep::Phone,
                epoch: 0,
            },
            &tx,
        );
        assert!(result.is_err(), "the phone is gone");
        let failed = rx.try_recv().expect("a failure event");
        assert!(
            matches!(failed, AdapterEvent::Login { epoch: 0, .. }),
            "stamped with the step's epoch, not raw: {failed:?}"
        );
        // Cancel's bump then makes the host drop it at delivery.
        epoch.fetch_add(1, Ordering::SeqCst);
        let AdapterEvent::Login { epoch: stamped, .. } = failed else {
            unreachable!()
        };
        assert_ne!(stamped, epoch.load(Ordering::SeqCst));
    }

    #[test]
    fn ready_side_effects_stop_at_cancel_and_roll_back_after_it() {
        let src = include_str!("tdlib.rs");
        // Inbox events stop at the worker once Cancel moves the epoch.
        let linked = &src[src.find("fn linked(&self)").expect("linked")..];
        let linked = &linked[..linked.find("\n    }").expect("end")];
        assert!(linked.contains("self.authorized && self.closing.current()"));
        assert!(fn_body(src, "fn apply_chat_update").contains("let emit = live.linked();"));
        assert!(src.contains("load_main_chats(client_id, live.linked(), &events)"));
        assert!(
            !src.contains("if !live.authorized"),
            "every inbox gate uses linked()"
        );
        let auth = fn_body(src, "async fn apply_authorization");
        let ready = &auth[auth.find("AuthorizationState::Ready").expect("ready")..];
        let ready = &ready[..ready
            .find("AuthorizationState::WaitEmailAddress")
            .expect("next arm")];
        let first_check = ready.find("if !login.open()").expect("check");
        let marker = ready.find("TDLIB_SESSION_MARKER").expect("marker");
        let load = ready.find("load_main_chats(").expect("chat load");
        assert!(first_check < marker);
        assert!(
            ready[marker..load].contains("if !login.open()"),
            "checked again right before the chat load"
        );
        let worker = fn_body(src, "fn spawn_tdlib_worker");
        let close = &worker[worker
            .find("TdlibCommand::Close { cancel }")
            .expect("close")..];
        assert!(close.contains("let signed_in = live.authorized || live.late_ready;"));
        assert!(close.contains("close_kind(signed_in, live.new_login, cancel)"));
        let skip = &ready[..marker];
        assert!(skip.contains("late_ready(live.new_login, live.close_cancel)"));
        assert!(skip.contains("LateReady::Wait => live.late_ready = true"));
        assert!(skip.contains("LateReady::LogOut => request_log_out(client_id).await"));
        let auth_states = &auth[..auth.find("AuthorizationState::Ready").expect("ready")];
        for state in [
            "WaitPhoneNumber =>",
            "WaitCode(state) =>",
            "WaitPassword(_) =>",
        ] {
            let arm = &auth_states[auth_states.find(state).expect(state)..];
            let arm = &arm[..arm.find("login.phase(").expect("phase")];
            assert!(arm.contains("live.new_login = true"), "{state}");
        }
        assert!(
            !ready.contains("new_login = true"),
            "a resumed session goes straight to Ready and is never logged out"
        );
        assert!(close.contains("set_secret(TelegramSecretKey::Session, \"\")"));
        assert!(close.contains("request_log_out(client_id)"));
        assert!(fn_body(src, "async fn request_log_out").contains("functions::log_out"));
        let shutdown = &src[src.find("pub fn shutdown(").expect("shutdown")..];
        assert!(
            shutdown.contains("self.stop_client(false)"),
            "shutdown keeps the session"
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
                    epoch: 0,
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
                request: 1,
            },
            &tx,
        );
        assert!(empty.expect_err("blank").to_string().contains("empty"));
        let send = adapter.handle(
            AdapterCommand::SendText {
                protocol: ProtocolId::Telegram,
                conversation_id: "telegram:42".into(),
                body: "hello".into(),
                request: 2,
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
            request: 3,
        };
        let debug = format!("{command:?}");
        assert!(debug.contains("hello"));
        assert!(!debug.contains("11111"));
        assert!(!debug.contains("hash-value"));
    }

    #[test]
    fn a_step_queued_before_cancel_does_not_emit_a_failure() {
        use std::sync::atomic::{AtomicU64, Ordering};

        let epoch = Arc::new(AtomicU64::new(0));
        let vault = Arc::new(MemorySecretVault::new());
        vault.set_secret(TelegramSecretKey::ApiId, "11111");
        vault.set_secret(TelegramSecretKey::ApiHash, "hash-value");
        vault.set_secret(TelegramSecretKey::Phone, "+15551234567");
        let mut adapter = TelegramAdapter::with_login_epoch(
            Arc::clone(&vault) as Arc<dyn TelegramSecretVault>,
            TelegramApiSource::from_build(),
            Arc::clone(&epoch),
        );
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        // Cancel cleared the phone and bumped the epoch before this step ran.
        vault.set_secret(TelegramSecretKey::Phone, "");
        epoch.store(1, Ordering::SeqCst);
        adapter
            .handle(
                AdapterCommand::TelegramAuth {
                    step: TelegramAuthStep::Phone,
                    epoch: 0,
                },
                &tx,
            )
            .expect("stale step is ignored");
        assert!(
            rx.try_recv().is_err(),
            "no Failed and no error status from a cancelled step"
        );

        let err = adapter
            .handle(
                AdapterCommand::TelegramAuth {
                    step: TelegramAuthStep::Phone,
                    epoch: 1,
                },
                &tx,
            )
            .expect_err("the current client still reports a missing phone");
        assert!(err.to_string().contains("phone"));
        assert!(!err.to_string().contains("+1555"));
        match rx.try_recv().expect("failed phase") {
            AdapterEvent::Login { epoch: 1, event }
                if matches!(
                    *event,
                    AdapterEvent::TelegramAuth {
                        phase: TelegramAuthPhase::Failed
                    }
                ) => {}
            other => panic!("expected Failed stamped with epoch 1, got {other:?}"),
        }
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
                    epoch: 0,
                },
                &tx,
            )
            .expect_err("nonnumeric");
        assert!(err.to_string().contains("must be a number"));
        assert!(!err.to_string().contains("not-a-number"));
        let event = rx.try_recv().expect("failed phase");
        match event {
            AdapterEvent::Login { epoch: 0, event }
                if matches!(
                    *event,
                    AdapterEvent::TelegramAuth {
                        phase: TelegramAuthPhase::Failed
                    }
                ) => {}
            other => panic!("expected Failed stamped with epoch 0, got {other:?}"),
        }
    }
}
