//! Telegram adapter on the official TDLib path (`tdlib-rs` preferred).
//!
//! Default builds use a compile-safe stub so CI does not need system TDLib
//! or secrets. Enable `telegram-tdlib` later to compile the binding hook.
//! Read `api_id` / `api_hash` from a local secret store or environment —
//! never commit them.

use super::adapter::{
    emit_conversation, emit_message, emit_status, AdapterCommand, AdapterError, AdapterStatus,
    ChatMessage, Conversation, EventTx, ProtocolAdapter, ProtocolCapabilities, ProtocolId,
    SupportClass,
};

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
                preview: "Placeholder. TDLib is not connected.".into(),
            },
        );
        emit_conversation(
            events,
            Conversation {
                protocol: ProtocolId::Telegram,
                id: "telegram:family".into(),
                title: "Family".into(),
                preview: "Official TDLib path — stub only.".into(),
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

#[cfg(feature = "telegram-tdlib")]
mod tdlib_hook {
    /// Reserved hook for `tdlib-rs`.
    ///
    /// Next step (not in this revision): optional `tdlib-rs` dependency,
    /// `build.rs` via `tdlib_rs::build::build`, and a client loop that maps
    /// TDLib updates onto `AdapterEvent`. Do not start a login from CI.
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
}
