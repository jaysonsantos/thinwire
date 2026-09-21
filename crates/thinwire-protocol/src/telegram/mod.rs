//! Telegram adapter on the official TDLib path (`tdlib-rs`).
//!
//! Default builds keep a compile-safe auth state machine so CI does not need
//! system TDLib. Enable `telegram-tdlib` to compile the live client. Read
//! credentials from [`TelegramSecretVault`] — never put them on commands, never
//! log them, never commit them.

mod credentials;
mod engine;

#[cfg(feature = "telegram-tdlib")]
mod tdlib;

use std::fmt;
use std::sync::Arc;

use engine::TelegramAuthEngine;

pub use credentials::{
    TelegramApiOrigin, TelegramApiSource, resolve_telegram_api, telegram_api_available,
};

use super::adapter::{
    AdapterCommand, AdapterError, AdapterStatus, EventTx, ProtocolAdapter, ProtocolCapabilities,
    ProtocolId, SupportClass, TelegramAuthPhase, TelegramAuthStep, emit_status, emit_telegram_auth,
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
        let phase = self
            .engine
            .submit(step, self.secrets.as_ref(), &self.api_source)?;
        #[cfg(feature = "telegram-tdlib")]
        {
            self.tdlib.submit(
                step,
                Arc::clone(&self.secrets),
                self.api_source.clone(),
                events,
            );
            let _ = phase;
            return Ok(());
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
                emit_status(
                    events,
                    ProtocolId::Telegram,
                    AdapterStatus::Stubbed,
                    "Telegram disconnected.",
                );
                Ok(())
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

const fn command_mismatch(_command: AdapterCommand) -> &'static str {
    "command is not handled by the Telegram adapter"
}

const fn phase_status(phase: TelegramAuthPhase) -> AdapterStatus {
    match phase {
        TelegramAuthPhase::Ready => AdapterStatus::Ready,
        TelegramAuthPhase::Unavailable => AdapterStatus::Stubbed,
        TelegramAuthPhase::NeedPhone
        | TelegramAuthPhase::NeedCode
        | TelegramAuthPhase::NeedTwoFactor => AdapterStatus::Connecting,
    }
}

fn phase_detail(phase: TelegramAuthPhase, backend: &str) -> String {
    format!(
        "Telegram {phase} queued. Credentials stay in the secret store. {backend}",
        phase = phase.as_str()
    )
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
}
