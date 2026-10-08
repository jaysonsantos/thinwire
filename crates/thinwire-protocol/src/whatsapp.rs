//! MIT side of WhatsApp: the stub, the helper adapter, and the phone vault
//! that the UI and the worker share.
//!
//! The linked-device client links whatsapp-rust and its AGPL
//! `wacore-libsignal`. It lives in `thinwire-whatsapp` (AGPL-3.0-only, #77),
//! and it runs in the `thinwire-whatsapp-helper` process (ADR 0013). This
//! crate does not depend on it. A `whatsapp-web` build replaces this stub in
//! the host with [`whatsapp_helper_adapter`], which talks to that process
//! over a pipe. The stub answers every command the same way as that client
//! with the feature off.

use std::path::PathBuf;
use std::sync::Arc;

use crate::adapter::{
    AccountState, AdapterCommand, AdapterError, AdapterEvent, AdapterStatus, EventTx,
    ProtocolAdapter, ProtocolCapabilities, ProtocolId, SupportClass, emit_account,
    emit_chat_list_loaded, emit_history_loaded, emit_older_history_loaded, emit_send_rejected,
    emit_status, emit_stopped,
};

const CAPABILITY_DETAIL: &str = "Unofficial Web / linked-device style (whatsapp-rust). Experimental spike is off in this build. Ban risk.";

const CAPABILITIES: ProtocolCapabilities = ProtocolCapabilities {
    id: ProtocolId::WhatsApp,
    support: SupportClass::Experimental,
    short_label: "Experimental · ban risk",
    detail: CAPABILITY_DETAIL,
    official_api: false,
    allows_user_account_automation: false,
    // Only the local-only client can send or page history.
    sends_text: false,
    pages_history: false,
};

/// File name of the WhatsApp helper program, without `.exe`.
pub const WHATSAPP_HELPER_PROGRAM: &str = "thinwire-whatsapp-helper";

/// The capabilities of the client in the helper. The account row shows them
/// also while no helper runs.
const HELPER_CAPABILITIES: ProtocolCapabilities = ProtocolCapabilities {
    id: ProtocolId::WhatsApp,
    support: SupportClass::Experimental,
    short_label: "Experimental · ban risk",
    detail: "Unofficial Web / linked-device client in the thinwire-whatsapp-helper process. Experimental. Ban risk. Not a supported messenger.",
    official_api: false,
    allows_user_account_automation: false,
    sends_text: true,
    pages_history: true,
};

/// The WhatsApp adapter of a `whatsapp-web` build: it runs the AGPL client
/// in the `thinwire-whatsapp-helper` process (ADR 0013).
///
/// `phone` is the vault of the pairing screen. `configured` is the helper
/// path from the settings, used when no helper is next to the app binary.
#[must_use]
pub fn whatsapp_helper_adapter(
    phone: Arc<WhatsAppPhoneVault>,
    configured: Option<PathBuf>,
) -> crate::helper::HelperAdapter {
    crate::helper::HelperAdapter::new(
        crate::helper::HelperSpec {
            protocol: ProtocolId::WhatsApp,
            capabilities: HELPER_CAPABILITIES,
        },
        Arc::new(crate::helper::ProcessLauncher::new(
            WHATSAPP_HELPER_PROGRAM,
            configured,
        )),
        Some(phone),
    )
}

const NOT_CONNECTED: &str = "WhatsApp is not connected. Pass the ban gate and pair a device first.";

const RISK_GATE_REQUIRED: &str =
    "WhatsApp pairing is refused until the full-screen ban gate is accepted";

const FEATURE_OFF: &str = "whatsapp-web is off in this build. No QR or pair session is started.";

/// In-memory phone for an optional pair code.
///
/// The UI writes it. The worker reads it. [`AdapterCommand`] never carries it.
#[derive(Default)]
pub struct WhatsAppPhoneVault {
    phone: std::sync::Mutex<Option<String>>,
}

impl WhatsAppPhoneVault {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_phone(&self, value: &str) {
        let Ok(mut slot) = self.phone.lock() else {
            return;
        };
        let trimmed = value.trim();
        if trimmed.is_empty() {
            *slot = None;
        } else {
            *slot = Some(trimmed.to_string());
        }
    }

    #[must_use]
    pub fn phone(&self) -> Option<String> {
        self.phone.lock().ok().and_then(|slot| slot.clone())
    }

    pub fn clear(&self) {
        if let Ok(mut slot) = self.phone.lock() {
            *slot = None;
        }
    }
}

impl std::fmt::Debug for WhatsAppPhoneVault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WhatsAppPhoneVault")
            .field("phone", &"<redacted>")
            .finish()
    }
}

/// Feature-off WhatsApp adapter. It never opens a session.
pub struct WhatsAppAdapter {
    risk_acknowledged: bool,
}

impl WhatsAppAdapter {
    /// The vault stays with the host. Only the local-only client reads it.
    #[must_use]
    pub fn new(phone: Arc<WhatsAppPhoneVault>) -> Self {
        let _ = phone;
        Self {
            risk_acknowledged: false,
        }
    }

    #[must_use]
    pub const fn capabilities() -> ProtocolCapabilities {
        CAPABILITIES
    }

    fn acknowledge(&mut self, events: &EventTx) -> Result<(), AdapterError> {
        self.risk_acknowledged = true;
        emit_status(
            events,
            ProtocolId::WhatsApp,
            AdapterStatus::Stubbed,
            "WhatsApp ban gate accepted. No linked-device session has started.",
        );
        Ok(())
    }

    fn begin_link(&self) -> Result<(), AdapterError> {
        if !self.risk_acknowledged {
            return Err(AdapterError::Refused {
                protocol: ProtocolId::WhatsApp,
                reason: RISK_GATE_REQUIRED,
            });
        }
        Err(unavailable(FEATURE_OFF))
    }

    fn cancel_link(&mut self, events: &EventTx) -> Result<(), AdapterError> {
        self.risk_acknowledged = false;
        emit_account(events, ProtocolId::WhatsApp, AccountState::Unlinked);
        emit_status(
            events,
            ProtocolId::WhatsApp,
            AdapterStatus::Stubbed,
            "WhatsApp pairing cancelled. No linked-device session is running.",
        );
        Ok(())
    }
}

/// Report one failed command. The reason is a fixed text: no JID, no body.
fn command_failed(events: &EventTx, conversation_id: Option<String>) {
    let _ = events.send(AdapterEvent::CommandFailed {
        protocol: ProtocolId::WhatsApp,
        conversation_id,
        detail: NOT_CONNECTED.to_string(),
    });
}

const fn unavailable(reason: &'static str) -> AdapterError {
    AdapterError::Unavailable {
        protocol: ProtocolId::WhatsApp,
        reason,
    }
}

impl ProtocolAdapter for WhatsAppAdapter {
    fn id(&self) -> ProtocolId {
        ProtocolId::WhatsApp
    }

    fn capabilities(&self) -> ProtocolCapabilities {
        CAPABILITIES
    }

    fn start(&mut self, events: EventTx) {
        tracing::info!("whatsapp adapter start (experimental; no network)");
        emit_status(
            &events,
            ProtocolId::WhatsApp,
            AdapterStatus::Stubbed,
            CAPABILITIES.detail,
        );
    }

    /// Nothing runs: `Stopped` at once.
    fn shutdown(&mut self, events: &EventTx) {
        self.risk_acknowledged = false;
        emit_stopped(events, ProtocolId::WhatsApp);
    }

    /// ADR 0010 rules 4, 5 and 9: every command ends. A load ends with its
    /// answer, a send with `SendRejected`, and a failure with `CommandFailed`.
    fn handle(&mut self, command: AdapterCommand, events: &EventTx) -> Result<(), AdapterError> {
        match command {
            AdapterCommand::Connect {
                protocol: ProtocolId::WhatsApp,
            } => {
                emit_status(
                    events,
                    ProtocolId::WhatsApp,
                    AdapterStatus::Stubbed,
                    CAPABILITIES.detail,
                );
                Ok(())
            }
            AdapterCommand::LoadChats {
                protocol: ProtocolId::WhatsApp,
            } => {
                command_failed(events, None);
                emit_chat_list_loaded(events, ProtocolId::WhatsApp);
                Ok(())
            }
            AdapterCommand::OpenChat {
                protocol: ProtocolId::WhatsApp,
                conversation_id,
            } => {
                command_failed(events, Some(conversation_id.clone()));
                emit_history_loaded(events, ProtocolId::WhatsApp, conversation_id);
                Ok(())
            }
            AdapterCommand::SendText {
                protocol: ProtocolId::WhatsApp,
                conversation_id,
                request,
                ..
            }
            | AdapterCommand::ResendMessage {
                protocol: ProtocolId::WhatsApp,
                conversation_id,
                request,
                ..
            } => {
                emit_send_rejected(events, ProtocolId::WhatsApp, conversation_id, request);
                Ok(())
            }
            AdapterCommand::LoadOlderMessages {
                protocol: ProtocolId::WhatsApp,
                conversation_id,
                before_message_id,
            } => {
                emit_older_history_loaded(
                    events,
                    ProtocolId::WhatsApp,
                    conversation_id,
                    before_message_id,
                    false,
                    None,
                );
                Ok(())
            }
            AdapterCommand::Disconnect {
                protocol: ProtocolId::WhatsApp,
            }
            | AdapterCommand::WhatsAppCancelLink => self.cancel_link(events),
            AdapterCommand::WhatsAppAcknowledgeRisk => self.acknowledge(events),
            AdapterCommand::WhatsAppBeginLink { .. } => self.begin_link(),
            _ => Err(unavailable(
                "command is not handled by the WhatsApp adapter",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc::unbounded_channel;

    use super::*;

    fn stub() -> WhatsAppAdapter {
        WhatsAppAdapter::new(Arc::new(WhatsAppPhoneVault::new()))
    }

    fn drain(rx: &mut tokio::sync::mpsc::UnboundedReceiver<AdapterEvent>) -> Vec<AdapterEvent> {
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    /// #77: the stub keeps the ban gate. After the gate, pairing is off in
    /// this build.
    #[test]
    fn the_stub_refuses_pairing_before_the_gate_and_is_off_after_it() {
        let mut adapter = stub();
        let (tx, _rx) = unbounded_channel();
        let refused = adapter.handle(AdapterCommand::WhatsAppBeginLink { generation: 1 }, &tx);
        assert!(matches!(
            refused,
            Err(AdapterError::Refused { reason, .. }) if reason == RISK_GATE_REQUIRED
        ));
        adapter
            .handle(AdapterCommand::WhatsAppAcknowledgeRisk, &tx)
            .expect("gate");
        let off = adapter.handle(AdapterCommand::WhatsAppBeginLink { generation: 2 }, &tx);
        assert!(matches!(
            off,
            Err(AdapterError::Unavailable { reason, .. }) if reason == FEATURE_OFF
        ));
    }

    /// #77: every command of the shell ends, with the same answers as the
    /// client with the feature off (ADR 0010 rules 4, 5 and 9).
    #[test]
    fn the_stub_ends_every_shell_command() {
        let mut adapter = stub();
        let (tx, mut rx) = unbounded_channel();
        let chat = "whatsapp:111@s.whatsapp.net".to_string();
        for command in [
            AdapterCommand::LoadChats {
                protocol: ProtocolId::WhatsApp,
            },
            AdapterCommand::OpenChat {
                protocol: ProtocolId::WhatsApp,
                conversation_id: chat.clone(),
            },
            AdapterCommand::SendText {
                protocol: ProtocolId::WhatsApp,
                conversation_id: chat.clone(),
                body: "hello".into(),
                request: 7,
            },
            AdapterCommand::ResendMessage {
                protocol: ProtocolId::WhatsApp,
                conversation_id: chat.clone(),
                message_id: "pending:1".into(),
                request: 8,
            },
            AdapterCommand::LoadOlderMessages {
                protocol: ProtocolId::WhatsApp,
                conversation_id: chat.clone(),
                before_message_id: "m1".into(),
            },
        ] {
            adapter.handle(command, &tx).expect("no adapter error");
        }
        let events = drain(&mut rx);
        let failed = events
            .iter()
            .filter(|event| matches!(event, AdapterEvent::CommandFailed { .. }))
            .count();
        assert_eq!(failed, 2, "LoadChats and OpenChat fail as commands");
        assert!(events.contains(&AdapterEvent::ChatListLoaded {
            protocol: ProtocolId::WhatsApp
        }));
        assert!(events.contains(&AdapterEvent::HistoryLoaded {
            protocol: ProtocolId::WhatsApp,
            conversation_id: chat.clone(),
        }));
        for request in [7, 8] {
            assert!(events.contains(&AdapterEvent::SendRejected {
                protocol: ProtocolId::WhatsApp,
                conversation_id: chat.clone(),
                request,
            }));
        }
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AdapterEvent::OlderHistoryLoaded { more: false, .. }))
        );
    }

    /// #77: the stub has the feature-off capabilities and stops at once.
    #[tokio::test]
    async fn the_stub_follows_the_adapter_contract() {
        let adapter = stub();
        let caps = adapter.capabilities();
        assert!(!caps.sends_text && !caps.pages_history);
        assert_eq!(caps.detail, CAPABILITY_DETAIL);
        let mut kit = crate::contract::Contract::new(Box::new(adapter));
        kit.settle().await;
        kit.check_shutdown().await;
        kit.check_stream();
    }
}
