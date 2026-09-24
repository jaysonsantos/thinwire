//! WhatsApp unofficial linked-device adapter (ZapFast / whatsapp-rust).
//!
//! Experimental. ToS / ban risk. The default build keeps a stub and does not
//! open a network session. Feature `whatsapp-web` may pair only after the UI
//! has accepted the full-screen ban gate. Secrets never travel on commands.

mod path;

#[cfg(feature = "whatsapp-web")]
mod live;

use std::sync::Arc;

use super::adapter::{
    AdapterCommand, AdapterError, AdapterStatus, ChatMessage, Conversation, Delivery, EventTx,
    ProtocolAdapter, ProtocolCapabilities, ProtocolId, SupportClass, emit_conversation,
    emit_message, emit_status,
};

#[cfg(not(feature = "whatsapp-web"))]
const CAPABILITY_DETAIL: &str = "Unofficial Web / linked-device style (whatsapp-rust). Experimental spike is off in this build. Ban risk.";

#[cfg(feature = "whatsapp-web")]
const CAPABILITY_DETAIL: &str = "Unofficial Web / linked-device via whatsapp-rust. Experimental spike. Ban risk. Not a supported messenger.";

const CAPABILITIES: ProtocolCapabilities = ProtocolCapabilities {
    id: ProtocolId::WhatsApp,
    support: SupportClass::Experimental,
    short_label: "Experimental · ban risk",
    detail: CAPABILITY_DETAIL,
    official_api: false,
    allows_user_account_automation: false,
};

const RISK_GATE_REQUIRED: &str =
    "WhatsApp pairing is refused until the full-screen ban gate is accepted";

#[cfg(not(feature = "whatsapp-web"))]
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

/// Experimental WhatsApp adapter. Network pairing exists only with `whatsapp-web`
/// and only after [`AdapterCommand::WhatsAppAcknowledgeRisk`].
pub struct WhatsAppAdapter {
    risk_acknowledged: bool,
    /// Read when `whatsapp-web` starts pairing. Present in every build so the
    /// UI and the worker share one vault.
    #[cfg_attr(not(feature = "whatsapp-web"), allow(dead_code))]
    phone: Arc<WhatsAppPhoneVault>,
    #[cfg(feature = "whatsapp-web")]
    link: Arc<live::LiveLink>,
}

impl WhatsAppAdapter {
    #[must_use]
    pub fn new(phone: Arc<WhatsAppPhoneVault>) -> Self {
        Self {
            risk_acknowledged: false,
            phone,
            #[cfg(feature = "whatsapp-web")]
            link: Arc::new(live::LiveLink::new()),
        }
    }

    #[must_use]
    pub const fn capabilities() -> ProtocolCapabilities {
        CAPABILITIES
    }

    fn seed_placeholders(&self, events: &EventTx) {
        emit_status(
            events,
            ProtocolId::WhatsApp,
            AdapterStatus::Stubbed,
            CAPABILITIES.detail,
        );
        emit_conversation(
            events,
            Conversation {
                protocol: ProtocolId::WhatsApp,
                id: "whatsapp:placeholder".into(),
                title: "Placeholder chat".into(),
                participant: "Placeholder contact".into(),
                preview: "Experimental unofficial path — not connected.".into(),
                unread: 1,
                order: 0,
                last_at: 0,
                is_group: false,
            },
        );
        emit_message(
            events,
            ChatMessage {
                protocol: ProtocolId::WhatsApp,
                conversation_id: "whatsapp:placeholder".into(),
                id: "whatsapp:placeholder:1".into(),
                sender: "thinwire".into(),
                body: "WhatsApp is experimental. Unofficial linked-device code can get a personal account banned. This is not a live session.".into(),
                outbound: false,
                delivery: Delivery::Sent,
                sent_at: 0,
            },
        );
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

    fn begin_link(&mut self, events: &EventTx) -> Result<(), AdapterError> {
        if !self.risk_acknowledged {
            return Err(AdapterError::Refused {
                protocol: ProtocolId::WhatsApp,
                reason: RISK_GATE_REQUIRED,
            });
        }
        #[cfg(not(feature = "whatsapp-web"))]
        {
            let _ = events;
            Err(AdapterError::Unavailable {
                protocol: ProtocolId::WhatsApp,
                reason: FEATURE_OFF,
            })
        }
        #[cfg(feature = "whatsapp-web")]
        {
            let token = self.link.next_generation();
            self.link.mark_active();
            let link = Arc::clone(&self.link);
            let phone = self.phone.phone();
            let task_events = events.clone();
            tokio::spawn(async move {
                live::run_link(link, token, phone, task_events).await;
            });
            emit_status(
                events,
                ProtocolId::WhatsApp,
                AdapterStatus::Connecting,
                "Experimental WhatsApp pairing was queued on the worker. This is not a supported messenger.",
            );
            Ok(())
        }
    }

    fn cancel_link(&mut self, events: &EventTx) -> Result<(), AdapterError> {
        self.risk_acknowledged = false;
        #[cfg(feature = "whatsapp-web")]
        {
            self.link.next_generation();
            let link = Arc::clone(&self.link);
            tokio::spawn(async move {
                link.shutdown().await;
            });
        }
        emit_status(
            events,
            ProtocolId::WhatsApp,
            AdapterStatus::Stubbed,
            "WhatsApp pairing cancelled. No linked-device session is running.",
        );
        Ok(())
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
        self.seed_placeholders(&events);
    }

    /// Stop pairing, close the linked-device bot and its SQLite session, then
    /// `Stopped`. Without the spike feature nothing runs: `Stopped` at once.
    fn shutdown(&mut self, events: &EventTx) {
        self.risk_acknowledged = false;
        #[cfg(feature = "whatsapp-web")]
        {
            self.link.next_generation();
            let link = Arc::clone(&self.link);
            let events = events.clone();
            tokio::spawn(async move {
                link.shutdown().await;
                super::adapter::emit_stopped(&events, ProtocolId::WhatsApp);
            });
        }
        #[cfg(not(feature = "whatsapp-web"))]
        super::adapter::emit_stopped(events, ProtocolId::WhatsApp);
    }

    fn handle(&mut self, command: AdapterCommand, events: &EventTx) -> Result<(), AdapterError> {
        match command {
            AdapterCommand::Connect {
                protocol: ProtocolId::WhatsApp,
            } => {
                #[cfg(feature = "whatsapp-web")]
                if self.link.is_active() {
                    return Ok(());
                }
                emit_status(
                    events,
                    ProtocolId::WhatsApp,
                    AdapterStatus::Stubbed,
                    CAPABILITIES.detail,
                );
                Ok(())
            }
            AdapterCommand::Disconnect {
                protocol: ProtocolId::WhatsApp,
            }
            | AdapterCommand::WhatsAppCancelLink => self.cancel_link(events),
            AdapterCommand::WhatsAppAcknowledgeRisk => self.acknowledge(events),
            AdapterCommand::WhatsAppBeginLink => self.begin_link(events),
            _ => Err(AdapterError::Unavailable {
                protocol: ProtocolId::WhatsApp,
                reason: "command is not handled by the WhatsApp adapter",
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shutdown_closes_the_link_then_reports_stopped() {
        let mut adapter = WhatsAppAdapter::new(Arc::new(WhatsAppPhoneVault::new()));
        adapter.risk_acknowledged = true;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        adapter.shutdown(&tx);
        assert!(
            !adapter.risk_acknowledged,
            "the ban gate must be accepted again"
        );
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("Stopped in time")
            .expect("channel open");
        assert_eq!(
            event,
            AdapterEvent::Stopped {
                protocol: ProtocolId::WhatsApp
            }
        );
        let src = include_str!("mod.rs");
        let body = &src[src.find("fn shutdown(&mut self").expect("shutdown")..];
        let body = &body[..body.find("\n    }\n").expect("end")];
        let close = body.find("link.shutdown().await").expect("link closes");
        let stopped = body.find("emit_stopped(&events").expect("then Stopped");
        assert!(
            close < stopped,
            "Stopped only after the bot and its session close"
        );
    }
    use crate::adapter::{AdapterEvent, RedactedPairingSecret};
    use tokio::sync::mpsc::unbounded_channel;

    fn adapter() -> (
        WhatsAppAdapter,
        tokio::sync::mpsc::UnboundedReceiver<AdapterEvent>,
    ) {
        let (tx, rx) = unbounded_channel();
        let mut adapter = WhatsAppAdapter::new(Arc::new(WhatsAppPhoneVault::new()));
        adapter.start(tx);
        (adapter, rx)
    }

    fn drain(rx: &mut tokio::sync::mpsc::UnboundedReceiver<AdapterEvent>) -> Vec<AdapterEvent> {
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    fn assert_never_ready(events: &[AdapterEvent]) {
        for event in events {
            if let AdapterEvent::Status { status, .. } = event {
                assert_ne!(*status, AdapterStatus::Ready);
            }
        }
    }

    #[test]
    fn start_stays_stubbed() {
        let (_adapter, mut rx) = adapter();
        let events = drain(&mut rx);
        assert!(events.iter().any(|event| matches!(
            event,
            AdapterEvent::Status {
                status: AdapterStatus::Stubbed,
                ..
            }
        )));
        assert_never_ready(&events);
    }

    #[test]
    fn begin_link_without_risk_gate_is_refused_and_redacts_phone() {
        let phone = Arc::new(WhatsAppPhoneVault::new());
        phone.set_phone("+15551212999");
        let (tx, mut rx) = unbounded_channel();
        let mut adapter = WhatsAppAdapter::new(phone);
        let error = adapter
            .handle(AdapterCommand::WhatsAppBeginLink, &tx)
            .expect_err("gate");
        assert!(matches!(error, AdapterError::Refused { .. }));
        let debug = format!("{error:?} {:?}", drain(&mut rx));
        assert!(!debug.contains("15551212999"));
        assert!(!debug.contains("+1555"));
    }

    #[test]
    fn acknowledge_then_status_is_not_ready() {
        let (tx, mut rx) = unbounded_channel();
        let mut adapter = WhatsAppAdapter::new(Arc::new(WhatsAppPhoneVault::new()));
        adapter
            .handle(AdapterCommand::WhatsAppAcknowledgeRisk, &tx)
            .expect("ack");
        let events = drain(&mut rx);
        assert!(events.iter().any(|event| matches!(
            event,
            AdapterEvent::Status {
                status: AdapterStatus::Stubbed,
                ..
            }
        )));
        assert_never_ready(&events);
        let debug = format!("{events:?}");
        assert!(!debug.to_ascii_lowercase().contains("ready"));
    }

    #[cfg(not(feature = "whatsapp-web"))]
    #[test]
    fn feature_off_refuses_link_after_acknowledge() {
        let (tx, mut rx) = unbounded_channel();
        let phone = Arc::new(WhatsAppPhoneVault::new());
        phone.set_phone("15550001111");
        let mut adapter = WhatsAppAdapter::new(phone);
        adapter
            .handle(AdapterCommand::WhatsAppAcknowledgeRisk, &tx)
            .expect("ack");
        let error = adapter
            .handle(AdapterCommand::WhatsAppBeginLink, &tx)
            .expect_err("feature off");
        assert!(matches!(error, AdapterError::Unavailable { .. }));
        let events = drain(&mut rx);
        let debug = format!("{error:?} {events:?}");
        assert!(!debug.contains("15550001111"));
        assert_never_ready(&events);
    }

    #[test]
    fn connect_does_not_mark_ready() {
        let (tx, mut rx) = unbounded_channel();
        let mut adapter = WhatsAppAdapter::new(Arc::new(WhatsAppPhoneVault::new()));
        adapter
            .handle(
                AdapterCommand::Connect {
                    protocol: ProtocolId::WhatsApp,
                },
                &tx,
            )
            .expect("connect");
        let events = drain(&mut rx);
        assert_never_ready(&events);
        assert!(events.iter().any(|event| matches!(
            event,
            AdapterEvent::Status {
                status: AdapterStatus::Stubbed,
                ..
            }
        )));
    }

    #[test]
    fn commands_carry_no_pairing_material() {
        assert_eq!(
            format!("{:?}", AdapterCommand::WhatsAppBeginLink),
            "WhatsAppBeginLink"
        );
        assert_eq!(
            format!("{:?}", AdapterCommand::WhatsAppAcknowledgeRisk),
            "WhatsAppAcknowledgeRisk"
        );
        assert_eq!(
            format!("{:?}", AdapterCommand::WhatsAppCancelLink),
            "WhatsAppCancelLink"
        );
        let secret = RedactedPairingSecret::new("qr-secret-value");
        let event = AdapterEvent::WhatsAppQr {
            code: secret,
            generation: 1,
        };
        let debug = format!("{event:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("qr-secret-value"));
    }

    #[test]
    fn phone_vault_debug_is_redacted() {
        let vault = WhatsAppPhoneVault::new();
        vault.set_phone("  15557654321  ");
        assert_eq!(vault.phone().as_deref(), Some("15557654321"));
        let debug = format!("{vault:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("15557654321"));
        vault.clear();
        assert_eq!(vault.phone(), None);
    }

    #[test]
    fn device_store_path_is_under_app_data_not_the_crate() {
        let path = path::device_store_file(std::path::Path::new("/var/lib/thinwire-test"));
        assert_eq!(
            path,
            std::path::PathBuf::from("/var/lib/thinwire-test/thinwire/whatsapp/device.sqlite")
        );
        let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        assert!(!path.starts_with(manifest));
    }

    #[cfg(unix)]
    #[test]
    fn session_dir_is_user_only() {
        let dir = std::env::temp_dir().join(format!(
            "thinwire-wa-perm-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        path::prepare_session_dir(&dir).expect("dir");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&dir).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
