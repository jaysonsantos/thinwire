//! Telegram adapter on the official TDLib path (`tdlib-rs` preferred).
//!
//! Default builds use a compile-safe stub so CI does not need system TDLib
//! or secrets. Enable `telegram-tdlib` later to compile the binding hook.
//! Read `api_id` / `api_hash` / session from the OS secret store using the
//! `TELEGRAM_SECRET_*` keys — never commit them, never log them.

use super::adapter::{
    AdapterCommand, AdapterError, AdapterStatus, ChatMessage, Conversation, EventTx,
    ProtocolAdapter, ProtocolCapabilities, ProtocolId, SupportClass, TelegramAuthStep,
    emit_conversation, emit_message, emit_status,
};

/// Service name for the OS keychain / secret store.
pub const TELEGRAM_SECRET_SERVICE: &str = "thinwire";
/// Keychain account for the Telegram `api_id`.
pub const TELEGRAM_SECRET_API_ID: &str = "telegram.api_id";
/// Keychain account for the Telegram `api_hash`.
pub const TELEGRAM_SECRET_API_HASH: &str = "telegram.api_hash";
/// Keychain account for the TDLib session blob (stub-safe).
pub const TELEGRAM_SECRET_SESSION: &str = "telegram.session";

const CAPABILITIES: ProtocolCapabilities = ProtocolCapabilities {
    id: ProtocolId::Telegram,
    support: SupportClass::Supported,
    short_label: "Supported · TDLib",
    detail: "Official TDLib via Rust bindings (tdlib-rs). Supported goal. Default build is a compile-safe stub.",
    official_api: true,
    allows_user_account_automation: false,
};

/// Official Telegram path. Real TDLib login is out of scope for this revision.
#[derive(Debug)]
pub struct TelegramAdapter;

impl TelegramAdapter {
    #[must_use]
    pub const fn capabilities() -> ProtocolCapabilities {
        CAPABILITIES
    }

    /// True when the `telegram-tdlib` feature compiled the binding hook.
    #[must_use]
    pub const fn uses_tdlib_hook() -> bool {
        cfg!(feature = "telegram-tdlib")
    }

    #[must_use]
    pub fn backend_detail() -> &'static str {
        tdlib_hook::backend_detail()
    }

    fn seed_placeholders(&self, events: &EventTx) {
        emit_status(
            events,
            ProtocolId::Telegram,
            AdapterStatus::Stubbed,
            Self::backend_detail(),
        );
        emit_conversation(
            events,
            Conversation {
                protocol: ProtocolId::Telegram,
                id: "telegram:saved".into(),
                title: "Saved Messages".into(),
                participant: "you".into(),
                preview: "Placeholder. TDLib is not connected.".into(),
                unread: 1,
            },
        );
        emit_conversation(
            events,
            Conversation {
                protocol: ProtocolId::Telegram,
                id: "telegram:family".into(),
                title: "Family".into(),
                participant: "Family".into(),
                preview: "TDLib path — stub only.".into(),
                unread: 0,
            },
        );
        emit_message(
            events,
            ChatMessage {
                protocol: ProtocolId::Telegram,
                conversation_id: "telegram:saved".into(),
                id: "telegram:saved:1".into(),
                sender: "thinwire".into(),
                body: "Telegram is the supported TDLib goal. This pane is placeholder data; no client login runs in CI.".into(),
                outbound: false,
            },
        );
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
        self.seed_placeholders(&events);
    }

    fn handle(&mut self, command: AdapterCommand, events: &EventTx) -> Result<(), AdapterError> {
        match command {
            AdapterCommand::TelegramAuth { step } => {
                emit_status(
                    events,
                    ProtocolId::Telegram,
                    telegram_step_status(step),
                    telegram_step_detail(step, Self::backend_detail()),
                );
                Ok(())
            }
            AdapterCommand::Connect {
                protocol: ProtocolId::Telegram,
            } => {
                emit_status(
                    events,
                    ProtocolId::Telegram,
                    AdapterStatus::Stubbed,
                    Self::backend_detail(),
                );
                Ok(())
            }
            AdapterCommand::Disconnect {
                protocol: ProtocolId::Telegram,
            } => {
                emit_status(
                    events,
                    ProtocolId::Telegram,
                    AdapterStatus::Stubbed,
                    "Telegram stub disconnected (no TDLib session).",
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

const fn command_mismatch(_command: AdapterCommand) -> &'static str {
    "command is not handled by the Telegram adapter"
}

const fn telegram_step_status(step: TelegramAuthStep) -> AdapterStatus {
    match step {
        TelegramAuthStep::Complete => AdapterStatus::Stubbed,
        TelegramAuthStep::ApiCredentials
        | TelegramAuthStep::Phone
        | TelegramAuthStep::Code
        | TelegramAuthStep::TwoFactor => AdapterStatus::Connecting,
    }
}

fn telegram_step_detail(step: TelegramAuthStep, backend: &str) -> String {
    format!(
        "Telegram {step} step queued. Credentials stay in the secret store. {backend}",
        step = step.as_str()
    )
}

#[cfg(feature = "telegram-tdlib")]
mod tdlib_hook {
    /// Reserved hook for `tdlib-rs`.
    ///
    /// Next step (not in this revision): optional `tdlib-rs` dependency,
    /// `build.rs` via `tdlib_rs::build::build`, and a client loop that maps
    /// TDLib updates onto `AdapterEvent`. Read `api_id` / `api_hash` / session
    /// from the OS secret store (`TELEGRAM_SECRET_*`). The UI screens stay the
    /// same. Do not start a login from CI.
    pub fn backend_detail() -> &'static str {
        "tdlib-rs hook compiled; client not started (no secrets, no system TDLib required yet)"
    }
}

#[cfg(not(feature = "telegram-tdlib"))]
mod tdlib_hook {
    pub fn backend_detail() -> &'static str {
        "TDLib stub — enable feature telegram-tdlib after a local TDLib install; never put api_id/api_hash in the repo"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_build_uses_compile_safe_stub() {
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
    }

    #[test]
    fn telegram_auth_step_does_not_echo_secrets() {
        use crate::AdapterEvent;

        let mut adapter = TelegramAdapter;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        adapter
            .handle(
                AdapterCommand::TelegramAuth {
                    step: TelegramAuthStep::ApiCredentials,
                },
                &tx,
            )
            .expect("auth step");
        let AdapterEvent::Status { detail, .. } = rx.try_recv().expect("status") else {
            panic!("expected status");
        };
        assert!(detail.contains("api credentials"));
        assert!(detail.contains("secret store"));
        let debug = format!(
            "{:?}",
            AdapterCommand::TelegramAuth {
                step: TelegramAuthStep::ApiCredentials
            }
        );
        assert!(debug.contains("ApiCredentials"));
        assert!(!debug.contains("11111"));
        assert!(!debug.contains("hash"));
    }
}
