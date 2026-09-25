//! WhatsApp unofficial linked-device adapter (ZapFast / whatsapp-rust).
//!
//! Experimental. ToS / ban risk. The default build keeps a stub and does not
//! open a network session. Feature `whatsapp-web` may pair only after the UI
//! has accepted the full-screen ban gate. Secrets never travel on commands.

mod inbox;
mod link;
mod path;
mod session;

#[cfg(feature = "whatsapp-web")]
mod live;

use std::sync::Arc;

use super::adapter::{
    AccountState, AdapterCommand, AdapterError, AdapterEvent, AdapterStatus, ChatMessage,
    Conversation, Delivery, EventTx, ProtocolAdapter, ProtocolCapabilities, ProtocolId,
    SupportClass, emit_chat_list_loaded, emit_conversation, emit_history_loaded, emit_message,
    emit_older_history_loaded, emit_send_accepted, emit_send_rejected, emit_status,
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
    // Only the live spike can send. The default build has no client.
    sends_text: cfg!(feature = "whatsapp-web"),
    // The session history pages from memory (LoadOlderMessages).
    pages_history: cfg!(feature = "whatsapp-web"),
};

/// Conversation id of the offline placeholder row. Not a real chat.
pub(crate) const PLACEHOLDER_ID: &str = "whatsapp:placeholder";

const STOP_TIMEOUT: &str =
    "The WhatsApp client did not stop in time. The app closes at its own limit.";

const NOT_CONNECTED: &str = "WhatsApp is not connected. Pass the ban gate and pair a device first.";
const BAD_CHAT_ID: &str = "that conversation is not a WhatsApp chat";
const UNKNOWN_CHAT: &str = "that WhatsApp chat is not in the synced list";
const EMPTY_BODY: &str = "the message is empty";

const RISK_GATE_REQUIRED: &str =
    "WhatsApp pairing is refused until the full-screen ban gate is accepted";

#[cfg_attr(feature = "whatsapp-web", allow(dead_code))]
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
    session: session::Session,
    /// The link lifecycle owner. Spawned on the first pairing. `None` means
    /// that no client ever ran. Tests put a fake backend here.
    link: Option<link::LinkHandle>,
}

impl WhatsAppAdapter {
    #[must_use]
    pub fn new(phone: Arc<WhatsAppPhoneVault>) -> Self {
        Self {
            risk_acknowledged: false,
            phone,
            session: session::Session::default(),
            link: None,
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
                id: PLACEHOLDER_ID.into(),
                title: "Placeholder chat".into(),
                participant: "Placeholder contact".into(),
                preview: "Experimental unofficial path — not connected.".into(),
                unread: 1,
                order: 0,
                last_at: 0,
                is_group: false,
                writable: false,
                placeholder: true,
            },
        );
        emit_message(
            events,
            ChatMessage {
                protocol: ProtocolId::WhatsApp,
                conversation_id: PLACEHOLDER_ID.into(),
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

    /// `pairing` is the shell's id for this pairing. Every QR code and pair
    /// code of it carries the same number (ADR 0010 rule 8).
    fn begin_link(&mut self, pairing: u64, events: &EventTx) -> Result<(), AdapterError> {
        if !self.risk_acknowledged {
            return Err(AdapterError::Refused {
                protocol: ProtocolId::WhatsApp,
                reason: RISK_GATE_REQUIRED,
            });
        }
        #[cfg(feature = "whatsapp-web")]
        if self.link.is_none() {
            self.link = Some(link::LinkHandle::spawn(
                live::LiveBackend,
                self.session.clone(),
                events.clone(),
            ));
        }
        let Some(link) = &self.link else {
            return Err(AdapterError::Unavailable {
                protocol: ProtocolId::WhatsApp,
                reason: FEATURE_OFF,
            });
        };
        link.begin(self.phone.phone(), pairing);
        emit_status(
            events,
            ProtocolId::WhatsApp,
            AdapterStatus::Connecting,
            "Experimental WhatsApp pairing was queued on the worker. This is not a supported messenger.",
        );
        Ok(())
    }

    fn cancel_link(&mut self, events: &EventTx) -> Result<(), AdapterError> {
        self.risk_acknowledged = false;
        // The owner stops the client and drops its later callbacks. The session
        // reset also refuses them, so the removals below are final.
        if let Some(link) = &self.link {
            link.cancel();
        }
        for event in self.session.reset() {
            let _ = events.send(event);
        }
        let _ = events.send(session::account(AccountState::Unlinked));
        emit_status(
            events,
            ProtocolId::WhatsApp,
            AdapterStatus::Stubbed,
            "WhatsApp pairing cancelled. No linked-device session is running.",
        );
        Ok(())
    }
}

impl WhatsAppAdapter {
    /// `Stopped` means that no WhatsApp client runs (#44). The owner stops
    /// the client after any start in progress, and the wait has its own bound
    /// below the app close limit. If that bound runs out, the client may
    /// still run: send an error, not `Stopped`. The app then closes at its
    /// own limit.
    fn finish_shutdown(stopped: bool, events: &EventTx) {
        if stopped {
            super::adapter::emit_stopped(events, ProtocolId::WhatsApp);
        } else {
            emit_status(
                events,
                ProtocolId::WhatsApp,
                AdapterStatus::Error,
                STOP_TIMEOUT,
            );
        }
    }

    fn connect(&self, events: &EventTx) {
        if self.session.is_connected() {
            emit_status(
                events,
                ProtocolId::WhatsApp,
                AdapterStatus::Ready,
                session::CONNECTED,
            );
            let _ = events.send(session::account(AccountState::Linked));
            self.load_chats(events);
            return;
        }
        if self.link.as_ref().is_some_and(link::LinkHandle::is_active) {
            return;
        }
        if let Some(detail) = self.session.stopped() {
            emit_status(events, ProtocolId::WhatsApp, AdapterStatus::Error, detail);
            return;
        }
        emit_status(
            events,
            ProtocolId::WhatsApp,
            AdapterStatus::Stubbed,
            CAPABILITIES.detail,
        );
    }

    /// The first chat-list page (connect, Refresh while connected).
    fn load_chats(&self, events: &EventTx) {
        for event in self.session.with_inbox(inbox::Inbox::chat_page) {
            let _ = events.send(event);
        }
    }

    /// The next chat-list page (`LoadChats`: "load another page"). After the
    /// last page it starts again at the first one, so a Refresh still sends
    /// the newest chats.
    fn load_more_chats(&self, events: &EventTx) {
        let page = self.session.with_inbox(|inbox| {
            let page = inbox.next_chat_page();
            if page.is_empty() {
                inbox.chat_page()
            } else {
                page
            }
        });
        for event in page {
            let _ = events.send(event);
        }
    }

    fn require_connected(&self) -> Result<(), AdapterError> {
        if self.session.is_connected() {
            Ok(())
        } else {
            Err(unavailable(NOT_CONNECTED))
        }
    }

    fn open_chat(&self, conversation_id: &str, events: &EventTx) -> Result<(), AdapterError> {
        self.require_connected()?;
        let jid = inbox::parse_conversation_id(conversation_id)
            .ok_or_else(|| unavailable(BAD_CHAT_ID))?;
        let opened = self
            .session
            .with_inbox(|inbox| inbox.open_chat(jid))
            .ok_or_else(|| unavailable(UNKNOWN_CHAT))?;
        for event in opened {
            let _ = events.send(event);
        }
        Ok(())
    }

    /// Accept or reject a send. `SendAccepted` follows the pending row;
    /// `SendRejected` means no row exists.
    ///
    /// A rejection is about this one send, so it does not return `Err`. An
    /// `Err` makes the host report an Error status, and the UI then unlinks
    /// the account and hides the inbox until the next Ready.
    fn send_text(&self, conversation_id: &str, body: &str, request: u64, events: &EventTx) {
        let Ok((jid, body, sender)) = self.prepare_send(conversation_id, body) else {
            emit_send_rejected(events, ProtocolId::WhatsApp, conversation_id, request);
            return;
        };
        let (pending, shown, upsert) = self
            .session
            .with_inbox(|inbox| inbox.begin_send(&jid, &body, unix_now()));
        let _ = events.send(shown);
        emit_send_accepted(events, ProtocolId::WhatsApp, conversation_id, request);
        // The sidebar preview and order come from the chat upsert.
        if let Some(upsert) = upsert {
            let _ = events.send(upsert);
        }
        self.spawn_send(sender, jid, pending, body, events);
    }

    fn prepare_send(
        &self,
        conversation_id: &str,
        body: &str,
    ) -> Result<(String, String, session::LinkSender), AdapterError> {
        let jid = inbox::parse_conversation_id(conversation_id)
            .ok_or_else(|| unavailable(BAD_CHAT_ID))?
            .to_string();
        let body = body.trim().to_string();
        if body.is_empty() {
            return Err(unavailable(EMPTY_BODY));
        }
        let sender = self
            .session
            .sender()
            .ok_or_else(|| unavailable(NOT_CONNECTED))?;
        if !self.session.with_inbox(|inbox| inbox.knows_chat(&jid)) {
            return Err(unavailable(UNKNOWN_CHAT));
        }
        Ok((jid, body, sender))
    }

    /// Older rows come from the in-memory history of this session. The
    /// request always ends with `OlderHistoryLoaded`, never with an error:
    /// an adapter error would mark the account Error and hide the inbox.
    fn load_older(&self, conversation_id: &str, before: &str, events: &EventTx) {
        let (page, more) = match inbox::parse_conversation_id(conversation_id) {
            Some(jid) if self.session.is_connected() => self
                .session
                .with_inbox(|inbox| inbox.older_page(jid, before, inbox::OPEN_CHAT_MESSAGES)),
            _ => (Vec::new(), false),
        };
        for event in page {
            let _ = events.send(event);
        }
        emit_older_history_loaded(
            events,
            ProtocolId::WhatsApp,
            conversation_id,
            before,
            more,
            None,
        );
    }

    /// Send a failed row again under the same local id.
    ///
    /// If the resend cannot start (offline, or the row is not a failed send),
    /// the row stays Failed, the account stays linked, and the shell gets
    /// `SendRejected` for the request.
    fn resend(&self, conversation_id: &str, message_id: &str, request: u64, events: &EventTx) {
        let started = inbox::parse_conversation_id(conversation_id)
            .map(str::to_string)
            .zip(self.session.sender())
            .and_then(|(jid, sender)| {
                let (body, event) = self
                    .session
                    .with_inbox(|inbox| inbox.retry_send(&jid, message_id))?;
                Some((jid, sender, body, event))
            });
        // ADR 0010 rule 4: every retry ends with SendAccepted or SendRejected.
        let Some((jid, sender, body, event)) = started else {
            emit_send_rejected(events, ProtocolId::WhatsApp, conversation_id, request);
            return;
        };
        let _ = events.send(event);
        emit_send_accepted(events, ProtocolId::WhatsApp, conversation_id, request);
        self.spawn_send(sender, jid, message_id.to_string(), body, events);
    }

    fn spawn_send(
        &self,
        (generation, sender): session::LinkSender,
        jid: String,
        pending: String,
        body: String,
        events: &EventTx,
    ) {
        let session = self.session.clone();
        let events = events.clone();
        let owner = self.link.as_ref().map(|link| link.callbacks(generation));
        tokio::spawn(async move {
            let result = sender.send_text(&jid, &body).await;
            let revoked = session.finish_send(generation, &jid, &pending, result, &events);
            // The phone revoked the device, maybe with no LoggedOut callback.
            // The owner stops the client and deletes the revoked store.
            if revoked && let Some(owner) = owner {
                owner.send(session::LinkEvent::LoggedOut);
            }
        });
    }
}

/// Chat JID inside a WhatsApp conversation id. The placeholder row returns `None`.
#[must_use]
pub fn parse_whatsapp_chat_id(conversation_id: &str) -> Option<&str> {
    inbox::parse_conversation_id(conversation_id)
}

/// Report one failed command. The reason is a fixed text: no JID, no body.
fn command_failed(events: &EventTx, conversation_id: Option<&str>, error: &AdapterError) {
    let detail = match error {
        AdapterError::Refused { reason, .. } | AdapterError::Unavailable { reason, .. } => *reason,
    };
    let _ = events.send(AdapterEvent::CommandFailed {
        protocol: ProtocolId::WhatsApp,
        conversation_id: conversation_id.map(str::to_string),
        detail: detail.to_string(),
    });
}

const fn unavailable(reason: &'static str) -> AdapterError {
    AdapterError::Unavailable {
        protocol: ProtocolId::WhatsApp,
        reason,
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
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
        let Some(link) = self.link.take() else {
            super::adapter::emit_stopped(events, ProtocolId::WhatsApp);
            return;
        };
        let events = events.clone();
        tokio::spawn(async move {
            Self::finish_shutdown(link.shutdown().await, &events);
        });
    }

    /// ADR 0010 rule 7. The viewed chat drives unread counts in the inbox.
    fn view_chat(&mut self, conversation_id: Option<&str>, events: &EventTx) {
        let jid = conversation_id.and_then(inbox::parse_conversation_id);
        if let Some(upsert) = self.session.with_inbox(|inbox| inbox.view(jid)) {
            let _ = events.send(upsert);
        }
    }

    fn handle(&mut self, command: AdapterCommand, events: &EventTx) -> Result<(), AdapterError> {
        match command {
            AdapterCommand::Connect {
                protocol: ProtocolId::WhatsApp,
            } => {
                self.connect(events);
                Ok(())
            }
            // ADR 0010 rule 5: a failed command keeps the session up. It ends
            // with CommandFailed, never an adapter error, and always ends the
            // shell's spinner (ChatListLoaded / HistoryLoaded).
            AdapterCommand::LoadChats {
                protocol: ProtocolId::WhatsApp,
            } => {
                match self.require_connected() {
                    Ok(()) => self.load_more_chats(events),
                    Err(error) => command_failed(events, None, &error),
                }
                emit_chat_list_loaded(events, ProtocolId::WhatsApp);
                Ok(())
            }
            AdapterCommand::OpenChat {
                protocol: ProtocolId::WhatsApp,
                conversation_id,
            } => {
                if let Err(error) = self.open_chat(&conversation_id, events) {
                    command_failed(events, Some(&conversation_id), &error);
                }
                emit_history_loaded(events, ProtocolId::WhatsApp, conversation_id);
                Ok(())
            }
            AdapterCommand::SendText {
                protocol: ProtocolId::WhatsApp,
                conversation_id,
                body,
                request,
            } => {
                self.send_text(&conversation_id, &body, request, events);
                Ok(())
            }
            AdapterCommand::LoadOlderMessages {
                protocol: ProtocolId::WhatsApp,
                conversation_id,
                before_message_id,
            } => {
                self.load_older(&conversation_id, &before_message_id, events);
                Ok(())
            }
            AdapterCommand::ResendMessage {
                protocol: ProtocolId::WhatsApp,
                conversation_id,
                message_id,
                request,
            } => {
                self.resend(&conversation_id, &message_id, request, events);
                Ok(())
            }
            AdapterCommand::Disconnect {
                protocol: ProtocolId::WhatsApp,
            }
            | AdapterCommand::WhatsAppCancelLink => self.cancel_link(events),
            AdapterCommand::WhatsAppAcknowledgeRisk => self.acknowledge(events),
            AdapterCommand::WhatsAppBeginLink { generation } => self.begin_link(generation, events),
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

        // With a running client, Stopped comes only after the client stopped.
        let (fake, handle, owner_session, _owner_rx) =
            link::tests::owner(link::tests::Fake::default());
        handle.begin(None, 1);
        handle.flush().await;
        let mut adapter = WhatsAppAdapter::new(Arc::new(WhatsAppPhoneVault::new()));
        adapter.session = owner_session;
        adapter.link = Some(handle);
        adapter.shutdown(&tx);
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
        assert_eq!(
            fake.log(),
            vec!["start 1", "stop 1"],
            "Stopped only after the client stopped"
        );
    }

    use crate::adapter::{AdapterEvent, RedactedPairingSecret};
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

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
            .handle(AdapterCommand::WhatsAppBeginLink { generation: 1 }, &tx)
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
            .handle(AdapterCommand::WhatsAppBeginLink { generation: 1 }, &tx)
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
            format!("{:?}", AdapterCommand::WhatsAppBeginLink { generation: 1 }),
            "WhatsAppBeginLink { generation: 1 }"
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

    use inbox::tests::message;
    use session::LinkEvent;
    use session::fake::FakeSender;

    const CHAT: &str = "111@s.whatsapp.net";

    fn with_sender(sender: Arc<FakeSender>) -> WhatsAppAdapter {
        let adapter = WhatsAppAdapter::new(Arc::new(WhatsAppPhoneVault::new()));
        adapter.session.begin(1);
        adapter.session.set_pairing(1);
        assert!(adapter.session.attach_sender(1, sender));
        adapter
    }

    fn history() -> LinkEvent {
        LinkEvent::History {
            chats: vec![inbox::HistoryChat {
                jid: CHAT.into(),
                name: Some("Ana".into()),
                unread: 3,
                timestamp: 10,
                messages: vec![
                    message(CHAT, "h1", "first", 9),
                    message(CHAT, "h2", "second", 10),
                ],
            }],
            push_names: Vec::new(),
        }
    }

    fn statuses(events: &[AdapterEvent]) -> Vec<AdapterStatus> {
        events
            .iter()
            .filter_map(|event| match event {
                AdapterEvent::Status { status, .. } => Some(*status),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn fake_pair_flow_reaches_ready_and_drops_the_placeholder() {
        let (tx, mut rx) = unbounded_channel();
        let adapter = with_sender(Arc::new(FakeSender::default()));
        let session = &adapter.session;
        session.apply(
            LinkEvent::Qr(RedactedPairingSecret::new("qr-secret")),
            1,
            &tx,
        );
        session.apply(
            LinkEvent::PairCode(RedactedPairingSecret::new("PAIR-1234")),
            1,
            &tx,
        );
        session.apply(LinkEvent::Paired, 1, &tx);
        assert!(!session.is_connected());
        session.apply(LinkEvent::Connected, 1, &tx);
        assert!(session.is_connected());
        let events = drain(&mut rx);
        assert!(matches!(
            &events[0],
            AdapterEvent::WhatsAppQr { generation: 1, code } if code.reveal() == "qr-secret"
        ));
        assert!(matches!(
            &events[1],
            AdapterEvent::WhatsAppPairCode { generation: 1, .. }
        ));
        assert_eq!(
            statuses(&events),
            vec![AdapterStatus::Connecting, AdapterStatus::Ready]
        );
        assert!(events.iter().any(|event| matches!(
            event,
            AdapterEvent::ConversationRemoved { id, .. } if id == PLACEHOLDER_ID
        )));
        let debug = format!("{events:?}");
        assert!(!debug.contains("qr-secret"));
        assert!(!debug.contains("PAIR-1234"));
        for event in &events {
            if let AdapterEvent::Status { detail, .. } = event {
                assert!(!detail.to_ascii_lowercase().contains("reliable"));
            }
        }
    }

    #[test]
    fn fake_chat_list_and_history_after_connect() {
        let (tx, mut rx) = unbounded_channel();
        let mut adapter = with_sender(Arc::new(FakeSender::default()));
        adapter.session.apply(history(), 1, &tx);
        adapter.session.apply(LinkEvent::Connected, 1, &tx);
        drain(&mut rx);

        adapter
            .handle(
                AdapterCommand::LoadChats {
                    protocol: ProtocolId::WhatsApp,
                },
                &tx,
            )
            .expect("load chats");
        let events = drain(&mut rx);
        assert_eq!(
            events.last(),
            Some(&AdapterEvent::ChatListLoaded {
                protocol: ProtocolId::WhatsApp
            }),
            "the list load ends the shell spinner"
        );
        assert!(events.iter().any(|event| matches!(
            event,
            AdapterEvent::ConversationUpsert { conversation }
                if conversation.id == "whatsapp:111@s.whatsapp.net"
                    && conversation.title == "Ana"
                    && conversation.unread == 3
                    && conversation.preview == "second"
        )));

        adapter
            .handle(
                AdapterCommand::OpenChat {
                    protocol: ProtocolId::WhatsApp,
                    conversation_id: "whatsapp:111@s.whatsapp.net".into(),
                },
                &tx,
            )
            .expect("open chat");
        let events = drain(&mut rx);
        let bodies: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                AdapterEvent::MessageReceived { message } => Some(message.body.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(bodies, vec!["first", "second"]);
        assert!(events.iter().any(|event| matches!(
            event,
            AdapterEvent::ConversationUpsert { conversation } if conversation.unread == 0
        )));

        adapter.session.apply(
            LinkEvent::Messages(vec![message(CHAT, "live1", "live text", 20)]),
            1,
            &tx,
        );
        let events = drain(&mut rx);
        assert!(events.iter().any(|event| matches!(
            event,
            AdapterEvent::MessageReceived { message } if message.body == "live text"
        )));
    }

    #[test]
    fn inbox_commands_refused_before_connect() {
        let (tx, mut rx) = unbounded_channel();
        let mut adapter = with_sender(Arc::new(FakeSender::default()));
        adapter.session.apply(history(), 1, &tx);
        drain(&mut rx);
        for command in [
            AdapterCommand::LoadChats {
                protocol: ProtocolId::WhatsApp,
            },
            AdapterCommand::OpenChat {
                protocol: ProtocolId::WhatsApp,
                conversation_id: "whatsapp:111@s.whatsapp.net".into(),
            },
            AdapterCommand::SendText {
                protocol: ProtocolId::WhatsApp,
                conversation_id: "whatsapp:111@s.whatsapp.net".into(),
                body: "hi".into(),
                request: 1,
            },
        ] {
            // ADR 0010 rule 5: no adapter error; the session stays up.
            assert!(adapter.handle(command, &tx).is_ok());
        }
        let events = drain(&mut rx);
        assert_eq!(
            events,
            vec![
                AdapterEvent::CommandFailed {
                    protocol: ProtocolId::WhatsApp,
                    conversation_id: None,
                    detail: NOT_CONNECTED.into(),
                },
                AdapterEvent::ChatListLoaded {
                    protocol: ProtocolId::WhatsApp
                },
                AdapterEvent::CommandFailed {
                    protocol: ProtocolId::WhatsApp,
                    conversation_id: Some("whatsapp:111@s.whatsapp.net".into()),
                    detail: NOT_CONNECTED.into(),
                },
                AdapterEvent::HistoryLoaded {
                    protocol: ProtocolId::WhatsApp,
                    conversation_id: "whatsapp:111@s.whatsapp.net".into(),
                },
                AdapterEvent::SendRejected {
                    protocol: ProtocolId::WhatsApp,
                    conversation_id: "whatsapp:111@s.whatsapp.net".into(),
                    request: 1,
                },
            ]
        );
        assert!(statuses(&events).is_empty(), "no Error status");
    }

    #[tokio::test]
    async fn fake_send_replaces_the_pending_row() {
        let (tx, mut rx) = unbounded_channel();
        let sender = Arc::new(FakeSender::default());
        let mut adapter = with_sender(Arc::clone(&sender));
        adapter.session.apply(history(), 1, &tx);
        adapter.session.apply(LinkEvent::Connected, 1, &tx);
        drain(&mut rx);

        adapter
            .handle(
                AdapterCommand::SendText {
                    protocol: ProtocolId::WhatsApp,
                    conversation_id: "whatsapp:111@s.whatsapp.net".into(),
                    body: "  hello  ".into(),
                    request: 1,
                },
                &tx,
            )
            .expect("send");
        let shown = rx.recv().await.expect("pending row");
        let pending = match shown {
            AdapterEvent::MessageReceived { message } => {
                assert!(message.outbound);
                assert_eq!(message.body, "hello");
                message.id
            }
            other => panic!("unexpected {other:?}"),
        };
        assert!(matches!(
            rx.recv().await.expect("accepted"),
            AdapterEvent::SendAccepted { request: 1, .. }
        ));
        assert!(matches!(
            rx.recv().await.expect("chat row"),
            AdapterEvent::ConversationUpsert { .. }
        ));
        let replaced = rx.recv().await.expect("replaced");
        assert!(matches!(
            replaced,
            AdapterEvent::MessageReplaced { old_id, message, .. }
                if old_id == pending && message.id == "SRV1"
        ));
        assert_eq!(
            sender.sent.lock().expect("lock").as_slice(),
            &[(CHAT.to_string(), "hello".to_string())]
        );
    }

    #[tokio::test]
    async fn fake_send_failure_marks_the_row_failed_and_resend_retries() {
        let (tx, mut rx) = unbounded_channel();
        let sender = Arc::new(FakeSender {
            fail: Some(session::SendFailure::Rejected),
            ..FakeSender::default()
        });
        let mut adapter = with_sender(sender);
        adapter.session.apply(history(), 1, &tx);
        adapter.session.apply(LinkEvent::Connected, 1, &tx);
        drain(&mut rx);

        adapter
            .handle(
                AdapterCommand::SendText {
                    protocol: ProtocolId::WhatsApp,
                    conversation_id: "whatsapp:111@s.whatsapp.net".into(),
                    body: "secret body".into(),
                    request: 1,
                },
                &tx,
            )
            .expect("queued");
        let pending = match rx.recv().await.expect("pending row") {
            AdapterEvent::MessageReceived { message } => {
                assert_eq!(message.delivery, Delivery::Pending);
                message.id
            }
            other => panic!("unexpected {other:?}"),
        };
        assert!(matches!(
            rx.recv().await.expect("accepted"),
            AdapterEvent::SendAccepted { request: 1, .. }
        ));
        assert!(matches!(
            rx.recv().await.expect("chat row"),
            AdapterEvent::ConversationUpsert { .. }
        ));
        let status = rx.recv().await.expect("status");
        match status {
            AdapterEvent::Status { status, detail, .. } => {
                assert_eq!(status, AdapterStatus::Error);
                assert!(!detail.contains("secret body"));
                assert!(!detail.contains("111"));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(
            rx.recv().await.expect("failed"),
            AdapterEvent::MessageDelivery { message_id, delivery: Delivery::Failed, .. }
                if message_id == pending
        ));

        adapter
            .handle(
                AdapterCommand::ResendMessage {
                    protocol: ProtocolId::WhatsApp,
                    conversation_id: "whatsapp:111@s.whatsapp.net".into(),
                    message_id: pending.clone(),
                    request: 2,
                },
                &tx,
            )
            .expect("resend");
        assert!(matches!(
            rx.recv().await.expect("pending again"),
            AdapterEvent::MessageDelivery {
                delivery: Delivery::Pending,
                ..
            }
        ));
        assert!(matches!(
            rx.recv().await.expect("retry accepted"),
            AdapterEvent::SendAccepted { request: 2, .. }
        ));
        let _status = rx.recv().await.expect("status");
        assert!(matches!(
            rx.recv().await.expect("failed again"),
            AdapterEvent::MessageDelivery {
                delivery: Delivery::Failed,
                ..
            }
        ));
        adapter
            .handle(
                AdapterCommand::ResendMessage {
                    protocol: ProtocolId::WhatsApp,
                    conversation_id: "whatsapp:111@s.whatsapp.net".into(),
                    message_id: "h1".into(),
                    request: 3,
                },
                &tx,
            )
            .expect("a resend of a history row is not an adapter error");
        assert_eq!(
            drain(&mut rx),
            vec![AdapterEvent::SendRejected {
                protocol: ProtocolId::WhatsApp,
                conversation_id: "whatsapp:111@s.whatsapp.net".into(),
                request: 3,
            }],
            "history rows are not failed sends"
        );
    }

    #[test]
    fn send_rejects_bad_ids_and_empty_bodies() {
        let (tx, mut rx) = unbounded_channel();
        let mut adapter = with_sender(Arc::new(FakeSender::default()));
        adapter.session.apply(history(), 1, &tx);
        adapter.session.apply(LinkEvent::Connected, 1, &tx);
        drain(&mut rx);
        for (id, body) in [
            (PLACEHOLDER_ID, "hi"),
            ("whatsapp:111@s.whatsapp.net", "   "),
            ("whatsapp:999@s.whatsapp.net", "hi"),
        ] {
            adapter
                .handle(
                    AdapterCommand::SendText {
                        protocol: ProtocolId::WhatsApp,
                        conversation_id: id.into(),
                        body: body.into(),
                        request: 1,
                    },
                    &tx,
                )
                .expect("a rejected send is not an adapter error");
        }
        let events = drain(&mut rx);
        assert_eq!(events.len(), 3);
        assert!(
            events
                .iter()
                .all(|event| matches!(event, AdapterEvent::SendRejected { request: 1, .. }))
        );
    }

    #[tokio::test]
    async fn logout_and_cancel_clear_the_inbox() {
        let (tx, mut rx) = unbounded_channel();
        let mut adapter = with_sender(Arc::new(FakeSender::default()));
        adapter.session.apply(history(), 1, &tx);
        adapter.session.apply(LinkEvent::Connected, 1, &tx);
        drain(&mut rx);
        adapter.session.apply(LinkEvent::LoggedOut, 1, &tx);
        let events = drain(&mut rx);
        assert!(!adapter.session.is_connected());
        assert!(events.iter().any(|event| matches!(
            event,
            AdapterEvent::ConversationRemoved { id, .. } if id == "whatsapp:111@s.whatsapp.net"
        )));
        assert_eq!(statuses(&events), vec![AdapterStatus::Error]);

        adapter.session.begin(2);
        assert!(
            adapter
                .session
                .attach_sender(2, Arc::new(FakeSender::default()))
        );
        adapter.session.apply(history(), 2, &tx);
        adapter.session.apply(LinkEvent::Connected, 2, &tx);
        drain(&mut rx);
        adapter
            .handle(AdapterCommand::WhatsAppCancelLink, &tx)
            .expect("cancel");
        assert!(!adapter.session.is_connected());
        let events = drain(&mut rx);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AdapterEvent::ConversationRemoved { .. }))
        );
    }

    #[test]
    fn connect_while_linked_reports_ready_and_the_list() {
        let (tx, mut rx) = unbounded_channel();
        let mut adapter = with_sender(Arc::new(FakeSender::default()));
        adapter.session.apply(history(), 1, &tx);
        adapter.session.apply(LinkEvent::Connected, 1, &tx);
        drain(&mut rx);
        adapter
            .handle(
                AdapterCommand::Connect {
                    protocol: ProtocolId::WhatsApp,
                },
                &tx,
            )
            .expect("connect");
        let events = drain(&mut rx);
        assert_eq!(statuses(&events), vec![AdapterStatus::Ready]);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AdapterEvent::ConversationUpsert { .. }))
        );
    }

    fn connected(
        sender: Arc<FakeSender>,
    ) -> (WhatsAppAdapter, UnboundedReceiver<AdapterEvent>, EventTx) {
        let (tx, mut rx) = unbounded_channel();
        let adapter = with_sender(sender);
        adapter.session.apply(history(), 1, &tx);
        adapter.session.apply(LinkEvent::Connected, 1, &tx);
        drain(&mut rx);
        (adapter, rx, tx)
    }

    fn send_hi() -> AdapterCommand {
        AdapterCommand::SendText {
            protocol: ProtocolId::WhatsApp,
            conversation_id: "whatsapp:111@s.whatsapp.net".into(),
            body: "hi".into(),
            request: 1,
        }
    }

    /// The pending row, then `SendAccepted` for request 1.
    async fn expect_pending_and_accepted(rx: &mut UnboundedReceiver<AdapterEvent>) -> String {
        let pending = match rx.recv().await.expect("pending row") {
            AdapterEvent::MessageReceived { message } => message.id,
            other => panic!("unexpected {other:?}"),
        };
        assert!(matches!(
            rx.recv().await.expect("accepted"),
            AdapterEvent::SendAccepted { request: 1, .. }
        ));
        assert!(matches!(
            rx.recv().await.expect("chat row"),
            AdapterEvent::ConversationUpsert { .. }
        ));
        pending
    }

    fn only_send_rejected(events: &[AdapterEvent]) -> bool {
        !events.is_empty()
            && events
                .iter()
                .all(|event| matches!(event, AdapterEvent::SendRejected { request: 1, .. }))
    }

    fn failing(failure: session::SendFailure) -> Arc<FakeSender> {
        Arc::new(FakeSender {
            fail: Some(failure),
            ..FakeSender::default()
        })
    }

    #[tokio::test]
    async fn network_error_on_send_marks_the_row_failed_and_keeps_the_link() {
        let (mut adapter, mut rx, tx) = connected(failing(session::SendFailure::Network));
        adapter.handle(send_hi(), &tx).expect("queued");
        let pending = expect_pending_and_accepted(&mut rx).await;
        match rx.recv().await.expect("status") {
            AdapterEvent::Status { status, detail, .. } => {
                assert_eq!(status, AdapterStatus::Error);
                assert_eq!(detail, session::SEND_NETWORK);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(
            rx.recv().await.expect("failed"),
            AdapterEvent::MessageDelivery { message_id, delivery: Delivery::Failed, .. }
                if message_id == pending
        ));
        assert!(adapter.session.is_connected());
        assert!(drain(&mut rx).is_empty());
    }

    #[tokio::test]
    async fn network_drop_refuses_send_until_reconnect() {
        let sender = Arc::new(FakeSender::default());
        let (mut adapter, mut rx, tx) = connected(Arc::clone(&sender));
        adapter.session.apply(LinkEvent::Disconnected, 1, &tx);
        let events = drain(&mut rx);
        assert_eq!(statuses(&events), vec![AdapterStatus::Connecting]);
        adapter
            .handle(send_hi(), &tx)
            .expect("a rejected send is not an adapter error");
        assert!(only_send_rejected(&drain(&mut rx)));
        assert!(sender.sent.lock().expect("lock").is_empty());

        adapter.session.apply(LinkEvent::Connected, 1, &tx);
        drain(&mut rx);
        adapter
            .handle(send_hi(), &tx)
            .expect("send after reconnect");
        let _pending = expect_pending_and_accepted(&mut rx).await;
        assert!(matches!(
            rx.recv().await.expect("replaced"),
            AdapterEvent::MessageReplaced { .. }
        ));
    }

    #[tokio::test]
    async fn revoked_device_on_send_clears_the_inbox() {
        let (mut adapter, mut rx, tx) = connected(failing(session::SendFailure::Unlinked));
        adapter.handle(send_hi(), &tx).expect("queued");
        let _pending = expect_pending_and_accepted(&mut rx).await;
        match rx.recv().await.expect("status") {
            AdapterEvent::Status { status, detail, .. } => {
                assert_eq!(status, AdapterStatus::Error);
                assert_eq!(detail, session::SEND_UNLINKED);
            }
            other => panic!("unexpected {other:?}"),
        }
        let mut rest = Vec::new();
        while rest.len() < 2 {
            rest.push(rx.recv().await.expect("event"));
        }
        assert!(matches!(
            rest[0],
            AdapterEvent::MessageDelivery {
                delivery: Delivery::Failed,
                ..
            }
        ));
        assert!(matches!(
            &rest[1],
            AdapterEvent::ConversationRemoved { id, .. } if id == "whatsapp:111@s.whatsapp.net"
        ));
        assert!(!adapter.session.is_connected());
        drain(&mut rx);
        adapter
            .handle(send_hi(), &tx)
            .expect("a rejected send is not an adapter error");
        assert!(only_send_rejected(&drain(&mut rx)));
    }

    #[test]
    fn revoked_device_drops_the_sender_even_if_connect_repeats() {
        let (mut adapter, mut rx, tx) = connected(Arc::new(FakeSender::default()));
        adapter.session.apply(LinkEvent::LoggedOut, 1, &tx);
        adapter.session.apply(LinkEvent::Connected, 1, &tx);
        drain(&mut rx);
        adapter.session.apply(history(), 1, &tx);
        drain(&mut rx);
        adapter
            .handle(send_hi(), &tx)
            .expect("a rejected send is not an adapter error");
        assert!(only_send_rejected(&drain(&mut rx)));
    }

    #[test]
    fn rate_limited_pair_code_is_an_error_without_pairing_material() {
        let (tx, mut rx) = unbounded_channel();
        let adapter = with_sender(Arc::new(FakeSender::default()));
        adapter.session.apply(LinkEvent::PairThrottled, 1, &tx);
        adapter.session.apply(LinkEvent::PairFailed, 1, &tx);
        adapter.session.apply(LinkEvent::QrExhausted, 1, &tx);
        let events = drain(&mut rx);
        let details: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                AdapterEvent::Status { status, detail, .. } => {
                    assert_eq!(*status, AdapterStatus::Error);
                    Some(detail.as_str())
                }
                AdapterEvent::Account {
                    state: AccountState::Unlinked,
                    ..
                } => None,
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(
            details,
            vec![
                session::PAIR_THROTTLED,
                session::PAIR_FAILED,
                session::QR_EXHAUSTED
            ]
        );
        assert!(!adapter.session.is_connected());
    }

    fn accounts(events: &[AdapterEvent]) -> Vec<AccountState> {
        events
            .iter()
            .filter_map(|event| match event {
                AdapterEvent::Account { state, .. } => Some(*state),
                _ => None,
            })
            .collect()
    }

    /// ADR 0010 rule 1: Linked before the first inbox event, Linking on a
    /// reconnect, Unlinked when the session ends.
    #[tokio::test]
    async fn account_events_follow_the_link_state() {
        let (tx, mut rx) = unbounded_channel();
        let mut adapter = with_sender(Arc::new(FakeSender::default()));
        adapter.session.apply(history(), 1, &tx);
        drain(&mut rx);
        adapter.session.apply(LinkEvent::Connected, 1, &tx);
        let events = drain(&mut rx);
        let linked = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    AdapterEvent::Account {
                        state: AccountState::Linked,
                        ..
                    }
                )
            })
            .expect("Linked");
        let first_row = events
            .iter()
            .position(|event| matches!(event, AdapterEvent::ConversationUpsert { .. }))
            .expect("rows");
        assert!(linked < first_row, "Linked comes before the inbox rows");

        adapter.session.apply(LinkEvent::Disconnected, 1, &tx);
        assert_eq!(accounts(&drain(&mut rx)), vec![AccountState::Linking]);
        adapter.session.apply(LinkEvent::Connected, 1, &tx);
        assert_eq!(accounts(&drain(&mut rx)), vec![AccountState::Linked]);

        // Refresh while linked repeats Linked.
        adapter
            .handle(
                AdapterCommand::Connect {
                    protocol: ProtocolId::WhatsApp,
                },
                &tx,
            )
            .expect("connect");
        assert_eq!(accounts(&drain(&mut rx)), vec![AccountState::Linked]);

        adapter
            .handle(AdapterCommand::WhatsAppCancelLink, &tx)
            .expect("cancel");
        assert_eq!(accounts(&drain(&mut rx)), vec![AccountState::Unlinked]);

        for event in [LinkEvent::LoggedOut, LinkEvent::TemporaryBan] {
            let (adapter, mut rx, tx) = connected(Arc::new(FakeSender::default()));
            adapter.session.apply(event, 1, &tx);
            assert_eq!(accounts(&drain(&mut rx)), vec![AccountState::Unlinked]);
        }
    }

    #[test]
    fn temporary_ban_stops_sends() {
        let (mut adapter, mut rx, tx) = connected(Arc::new(FakeSender::default()));
        adapter.session.apply(LinkEvent::TemporaryBan, 1, &tx);
        let events = drain(&mut rx);
        assert_eq!(statuses(&events), vec![AdapterStatus::Error]);
        adapter
            .handle(send_hi(), &tx)
            .expect("a rejected send is not an adapter error");
        assert!(only_send_rejected(&drain(&mut rx)));
        adapter
            .handle(
                AdapterCommand::LoadChats {
                    protocol: ProtocolId::WhatsApp,
                },
                &tx,
            )
            .expect("a failed load is not an adapter error");
        let events = drain(&mut rx);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AdapterEvent::CommandFailed { .. }))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AdapterEvent::ChatListLoaded { .. }))
        );
    }

    /// QA section 7 Low: a rejected send must not turn into an Error status.
    /// The host turns an adapter `Err` into `Status { Error }`, and the UI then
    /// unlinks WhatsApp and hides the inbox until the next Ready.
    #[tokio::test]
    async fn rejected_send_and_resend_keep_the_account_ready() {
        let (mut adapter, mut rx, tx) = connected(Arc::new(FakeSender::default()));
        adapter.session.apply(LinkEvent::Disconnected, 1, &tx);
        drain(&mut rx);
        for command in [
            send_hi(),
            AdapterCommand::SendText {
                protocol: ProtocolId::WhatsApp,
                conversation_id: PLACEHOLDER_ID.into(),
                body: "hi".into(),
                request: 1,
            },
            AdapterCommand::ResendMessage {
                protocol: ProtocolId::WhatsApp,
                conversation_id: "whatsapp:111@s.whatsapp.net".into(),
                message_id: "pending:9".into(),
                request: 1,
            },
        ] {
            assert!(adapter.handle(command, &tx).is_ok());
        }
        let events = drain(&mut rx);
        assert!(statuses(&events).is_empty(), "{events:?}");
        let rejected = events
            .iter()
            .filter(|event| matches!(event, AdapterEvent::SendRejected { request: 1, .. }))
            .count();
        assert_eq!(rejected, 3, "one rejection per send and per retry");
    }

    /// Codex r4093058040: a stale start that finishes late must not replace
    /// the sender of the current link.
    #[tokio::test]
    async fn stale_start_cannot_replace_the_current_sender() {
        let (tx, mut rx) = unbounded_channel();
        let adapter = WhatsAppAdapter::new(Arc::new(WhatsAppPhoneVault::new()));
        let current = Arc::new(FakeSender::default());
        let stale = Arc::new(FakeSender::default());
        adapter.session.begin(1);
        adapter.session.begin(2);
        assert!(adapter.session.attach_sender(2, Arc::clone(&current) as _));
        assert!(
            !adapter.session.attach_sender(1, Arc::clone(&stale) as _),
            "an older generation must not install its sender"
        );
        adapter.session.apply(history(), 2, &tx);
        adapter.session.apply(LinkEvent::Connected, 2, &tx);
        drain(&mut rx);
        let mut adapter = adapter;
        adapter.handle(send_hi(), &tx).expect("send");
        let _pending = expect_pending_and_accepted(&mut rx).await;
        assert!(matches!(
            rx.recv().await.expect("replaced"),
            AdapterEvent::MessageReplaced { .. }
        ));
        assert_eq!(current.sent.lock().expect("lock").len(), 1);
        assert!(stale.sent.lock().expect("lock").is_empty());
    }

    /// Codex r4093192847: after a cancel or a new pairing, events of the old
    /// link must not mark it Ready or refill the cleared inbox.
    #[test]
    fn stale_link_events_are_dropped_after_cancel_or_replace() {
        let (tx, mut rx) = unbounded_channel();
        let adapter = with_sender(Arc::new(FakeSender::default()));
        adapter.session.apply(history(), 1, &tx);
        assert!(!drain(&mut rx).is_empty());

        // Cancel: every event of link 1 is stale.
        let _ = adapter.session.reset();
        adapter.session.apply(history(), 1, &tx);
        adapter.session.apply(LinkEvent::Connected, 1, &tx);
        assert!(drain(&mut rx).is_empty());
        assert!(!adapter.session.is_connected());
        assert!(
            adapter
                .session
                .with_inbox(|inbox| inbox.chat_page().is_empty())
        );

        // Replace: link 2 runs, link 1 stays stale.
        adapter.session.begin(2);
        adapter.session.apply(LinkEvent::Connected, 1, &tx);
        assert!(drain(&mut rx).is_empty());
        adapter.session.apply(LinkEvent::Connected, 2, &tx);
        assert_eq!(statuses(&drain(&mut rx)), vec![AdapterStatus::Ready]);
    }

    /// Holds a send until the test releases it, then fails it.
    struct GatedSender {
        release: tokio::sync::Notify,
        failure: session::SendFailure,
    }

    impl session::WhatsAppSender for GatedSender {
        fn send_text<'a>(&'a self, _chat_jid: &'a str, _body: &'a str) -> session::SendFuture<'a> {
            Box::pin(async move {
                self.release.notified().await;
                Err(self.failure)
            })
        }
    }

    /// Codex r4093192850: a send of link 1 that ends after link 2 connected
    /// must not reset link 2 or touch its inbox.
    #[tokio::test]
    async fn late_send_result_of_an_old_link_is_dropped() {
        let (tx, mut rx) = unbounded_channel();
        let mut adapter = WhatsAppAdapter::new(Arc::new(WhatsAppPhoneVault::new()));
        let gated = Arc::new(GatedSender {
            release: tokio::sync::Notify::new(),
            failure: session::SendFailure::Unlinked,
        });
        adapter.session.begin(1);
        assert!(adapter.session.attach_sender(1, Arc::clone(&gated) as _));
        adapter.session.apply(history(), 1, &tx);
        adapter.session.apply(LinkEvent::Connected, 1, &tx);
        drain(&mut rx);
        adapter.handle(send_hi(), &tx).expect("send");
        let _pending = expect_pending_and_accepted(&mut rx).await;

        // The user cancels and pairs again while the send is in flight.
        adapter
            .handle(AdapterCommand::WhatsAppCancelLink, &tx)
            .expect("cancel");
        let current = Arc::new(FakeSender::default());
        adapter.session.begin(2);
        assert!(adapter.session.attach_sender(2, Arc::clone(&current) as _));
        adapter.session.apply(history(), 2, &tx);
        adapter.session.apply(LinkEvent::Connected, 2, &tx);
        drain(&mut rx);

        gated.release.notify_one();
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let late = drain(&mut rx);
        assert!(late.is_empty(), "old send result leaked: {late:?}");
        assert!(adapter.session.is_connected());

        adapter.handle(send_hi(), &tx).expect("send on link 2");
        let _pending = expect_pending_and_accepted(&mut rx).await;
        assert!(matches!(
            rx.recv().await.expect("replaced"),
            AdapterEvent::MessageReplaced { .. }
        ));
        assert_eq!(current.sent.lock().expect("lock").len(), 1);
    }

    /// #52 added LoadOlderMessages. WhatsApp pages its in-memory history and
    /// always ends with OlderHistoryLoaded, never with an adapter error.
    #[test]
    fn load_older_pages_history_and_never_errors() {
        let (tx, mut rx) = unbounded_channel();
        let mut adapter = with_sender(Arc::new(FakeSender::default()));
        let rows: Vec<_> = (0..120)
            .map(|n| message(CHAT, &format!("m{n:03}"), &format!("b{n}"), n))
            .collect();
        adapter.session.apply(
            LinkEvent::History {
                chats: vec![inbox::HistoryChat {
                    jid: CHAT.into(),
                    name: None,
                    unread: 0,
                    timestamp: 120,
                    messages: rows,
                }],
                push_names: Vec::new(),
            },
            1,
            &tx,
        );
        adapter.session.apply(LinkEvent::Connected, 1, &tx);
        drain(&mut rx);
        let older = |adapter: &mut WhatsAppAdapter, before: &str| {
            adapter
                .handle(
                    AdapterCommand::LoadOlderMessages {
                        protocol: ProtocolId::WhatsApp,
                        conversation_id: "whatsapp:111@s.whatsapp.net".into(),
                        before_message_id: before.into(),
                    },
                    &tx,
                )
                .expect("never an adapter error");
        };
        older(&mut adapter, "m070");
        let events = drain(&mut rx);
        let ids: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                AdapterEvent::MessageReceived { message } => Some(message.id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ids.len(), inbox::OPEN_CHAT_MESSAGES);
        assert_eq!(ids.first().copied(), Some("m020"));
        assert_eq!(ids.last().copied(), Some("m069"));
        assert!(matches!(
            events.last(),
            Some(AdapterEvent::OlderHistoryLoaded { more: true, .. })
        ));

        older(&mut adapter, "m020");
        assert!(matches!(
            drain(&mut rx).last(),
            Some(AdapterEvent::OlderHistoryLoaded { more: false, .. })
        ));

        // Unknown id and offline: no rows, no error, `more == false`.
        older(&mut adapter, "nope");
        assert_eq!(
            drain(&mut rx),
            vec![AdapterEvent::OlderHistoryLoaded {
                protocol: ProtocolId::WhatsApp,
                conversation_id: "whatsapp:111@s.whatsapp.net".into(),
                before_message_id: "nope".into(),
                more: false,
                note: None,
            }]
        );
        adapter.session.apply(LinkEvent::Disconnected, 1, &tx);
        drain(&mut rx);
        older(&mut adapter, "m070");
        let events = drain(&mut rx);
        assert_eq!(events.len(), 1);
        assert!(statuses(&events).is_empty());
    }

    /// ADR 0010 rule 7: ViewChat marks the viewed chat read, and leaving it
    /// lets new messages count again.
    #[test]
    fn view_chat_drives_unread_counts() {
        let (mut adapter, mut rx, tx) = connected(Arc::new(FakeSender::default()));
        adapter.view_chat(Some("whatsapp:111@s.whatsapp.net"), &tx);
        assert!(drain(&mut rx).iter().any(|event| matches!(
            event,
            AdapterEvent::ConversationUpsert { conversation } if conversation.unread == 0
        )));
        adapter.view_chat(None, &tx);
        adapter.session.apply(
            LinkEvent::Messages(vec![message(CHAT, "late", "late", 50)]),
            1,
            &tx,
        );
        assert!(drain(&mut rx).iter().any(|event| matches!(
            event,
            AdapterEvent::ConversationUpsert { conversation } if conversation.unread == 1
        )));
        // A placeholder or foreign id is ignored.
        adapter.view_chat(Some(PLACEHOLDER_ID), &tx);
        assert!(drain(&mut rx).is_empty());
    }

    /// Codex r4101563631: a send that shows a revoked device (Unlinked) with
    /// no LoggedOut callback still stops the client and deletes the store.
    #[tokio::test]
    async fn unlinked_send_result_stops_the_client_and_deletes_the_store() {
        let (fake, handle, owner_session, mut rx) =
            link::tests::owner(link::tests::Fake::default());
        let mut adapter = WhatsAppAdapter::new(Arc::new(WhatsAppPhoneVault::new()));
        adapter.session = owner_session;
        adapter.link = Some(handle);
        let link = adapter.link.as_ref().expect("link");
        link.begin(None, 1);
        link.flush().await;
        // The owner started generation 1; its client now reports the chat and
        // the connection. The send path of this client fails as Unlinked.
        assert!(
            adapter
                .session
                .attach_sender(1, failing(session::SendFailure::Unlinked))
        );
        fake.callback(1).send(history());
        fake.callback(1).send(LinkEvent::Connected);
        link.flush().await;
        drain(&mut rx);

        let (tx, _adapter_rx) = unbounded_channel();
        adapter.handle(send_hi(), &tx).expect("send");
        for _ in 0..200 {
            if fake.log().iter().any(|entry| entry == "delete") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(
            fake.log(),
            vec!["start 1", "mark revoked", "stop 1", "delete"],
            "the owner stops the client and deletes the revoked store"
        );
        assert!(!adapter.session.is_connected());
    }

    /// Codex r4093899833: `Stopped` only when the client really stopped.
    #[test]
    fn shutdown_timeout_sends_no_stopped() {
        let (tx, mut rx) = unbounded_channel();
        WhatsAppAdapter::finish_shutdown(false, &tx);
        let events = drain(&mut rx);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AdapterEvent::Stopped { .. }))
        );
        assert_eq!(statuses(&events), vec![AdapterStatus::Error]);
        WhatsAppAdapter::finish_shutdown(true, &tx);
        assert_eq!(
            drain(&mut rx),
            vec![AdapterEvent::Stopped {
                protocol: ProtocolId::WhatsApp
            }]
        );
    }

    /// The whole path: a client that never stops gets no `Stopped`.
    #[tokio::test]
    async fn shutdown_of_a_hanging_client_sends_no_stopped() {
        let (_fake, handle, owner_session, _owner_rx) = link::tests::owner(link::tests::Fake {
            stop_hangs: true,
            ..link::tests::Fake::default()
        });
        handle.begin(None, 1);
        handle.flush().await;
        let mut adapter = WhatsAppAdapter::new(Arc::new(WhatsAppPhoneVault::new()));
        adapter.session = owner_session;
        adapter.link = Some(handle);
        let (tx, mut rx) = unbounded_channel();
        adapter.shutdown(&tx);
        let event = tokio::time::timeout(
            link::SHUTDOWN_WAIT + std::time::Duration::from_secs(2),
            rx.recv(),
        )
        .await
        .expect("an answer after the bound")
        .expect("open channel");
        assert!(
            matches!(
                event,
                AdapterEvent::Status {
                    status: AdapterStatus::Error,
                    ..
                }
            ),
            "{event:?}"
        );
        assert!(rx.try_recv().is_err(), "no Stopped after the error");
    }

    /// Codex r4103855629: LoadChats pages the chat list; after the last page
    /// it starts again at the newest chats.
    #[test]
    fn load_chats_pages_then_starts_again() {
        let (tx, mut rx) = unbounded_channel();
        let mut adapter = with_sender(Arc::new(FakeSender::default()));
        let chats: Vec<inbox::HistoryChat> = (0..450)
            .map(|n| inbox::HistoryChat {
                jid: format!("{n}@s.whatsapp.net"),
                name: None,
                unread: 0,
                timestamp: n,
                messages: Vec::new(),
            })
            .collect();
        adapter.session.apply(
            LinkEvent::History {
                chats,
                push_names: Vec::new(),
            },
            1,
            &tx,
        );
        drain(&mut rx);
        let count = |events: &[AdapterEvent]| {
            events
                .iter()
                .filter(|event| matches!(event, AdapterEvent::ConversationUpsert { .. }))
                .count()
        };
        adapter.session.apply(LinkEvent::Connected, 1, &tx);
        assert_eq!(count(&drain(&mut rx)), 200, "the first page on connect");
        let mut load = |adapter: &mut WhatsAppAdapter| {
            adapter
                .handle(
                    AdapterCommand::LoadChats {
                        protocol: ProtocolId::WhatsApp,
                    },
                    &tx,
                )
                .expect("load chats");
            count(&drain(&mut rx))
        };
        assert_eq!(load(&mut adapter), 200);
        assert_eq!(load(&mut adapter), 50);
        assert_eq!(load(&mut adapter), 200, "the newest chats again");
    }

    /// ADR 0010 rules 9 and 10: pages_history only with the live client, and every
    /// LoadOlderMessages ends with exactly one OlderHistoryLoaded or
    /// CommandFailed for its chat, in every state. Never a Status.
    #[test]
    fn contract_every_older_request_gets_one_answer() {
        assert_eq!(
            WhatsAppAdapter::capabilities().pages_history,
            cfg!(feature = "whatsapp-web")
        );
        let chat = "whatsapp:111@s.whatsapp.net";
        let requests = [
            (chat, "h1"),
            (chat, "h2"),
            (chat, "unknown-message"),
            ("whatsapp:999@s.whatsapp.net", "x"),
            (PLACEHOLDER_ID, "whatsapp:placeholder:1"),
            ("telegram:1", "telegram:1:5"),
        ];
        let answers = |events: &[AdapterEvent], id: &str| {
            events
                .iter()
                .filter(|event| match event {
                    AdapterEvent::OlderHistoryLoaded {
                        conversation_id, ..
                    } => conversation_id == id,
                    AdapterEvent::CommandFailed {
                        conversation_id, ..
                    } => conversation_id.as_deref() == Some(id),
                    _ => false,
                })
                .count()
        };
        for connected in [false, true] {
            let (tx, mut rx) = unbounded_channel();
            let mut adapter = with_sender(Arc::new(FakeSender::default()));
            adapter.session.apply(history(), 1, &tx);
            if connected {
                adapter.session.apply(LinkEvent::Connected, 1, &tx);
            }
            drain(&mut rx);
            for (id, before) in requests {
                adapter
                    .handle(
                        AdapterCommand::LoadOlderMessages {
                            protocol: ProtocolId::WhatsApp,
                            conversation_id: id.into(),
                            before_message_id: before.into(),
                        },
                        &tx,
                    )
                    .expect("never an adapter error");
                let events = drain(&mut rx);
                assert_eq!(
                    answers(&events, id),
                    1,
                    "connected={connected} {id} {before}: {events:?}"
                );
                assert!(statuses(&events).is_empty(), "no Status for a page");
            }
        }
    }

    #[test]
    fn terminal_events_stop_the_link_and_invalidate_only_on_logout() {
        for event in [
            LinkEvent::LoggedOut,
            LinkEvent::TemporaryBan,
            LinkEvent::PairFailed,
            LinkEvent::PairThrottled,
            LinkEvent::QrExhausted,
        ] {
            assert!(event.stops_link(), "{event:?}");
            assert_eq!(
                event.invalidates_device(),
                event == LinkEvent::LoggedOut,
                "{event:?}"
            );
        }
        for event in [
            LinkEvent::Paired,
            LinkEvent::Connected,
            LinkEvent::Disconnected,
            LinkEvent::Messages(Vec::new()),
        ] {
            assert!(!event.stops_link(), "{event:?}");
            assert!(!event.invalidates_device(), "{event:?}");
        }
    }

    #[test]
    fn refresh_after_a_ban_keeps_the_ban_error() {
        let (mut adapter, mut rx, tx) = connected(Arc::new(FakeSender::default()));
        adapter.session.apply(LinkEvent::TemporaryBan, 1, &tx);
        drain(&mut rx);
        adapter
            .handle(
                AdapterCommand::Connect {
                    protocol: ProtocolId::WhatsApp,
                },
                &tx,
            )
            .expect("connect");
        match drain(&mut rx).as_slice() {
            [AdapterEvent::Status { status, detail, .. }] => {
                assert_eq!(*status, AdapterStatus::Error);
                assert_eq!(detail, session::TEMPORARY_BAN);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn cancel_clears_the_stop_reason() {
        let (mut adapter, mut rx, tx) = connected(Arc::new(FakeSender::default()));
        adapter.session.apply(LinkEvent::LoggedOut, 1, &tx);
        adapter
            .handle(AdapterCommand::WhatsAppCancelLink, &tx)
            .expect("cancel");
        drain(&mut rx);
        adapter
            .handle(
                AdapterCommand::Connect {
                    protocol: ProtocolId::WhatsApp,
                },
                &tx,
            )
            .expect("connect");
        assert_eq!(statuses(&drain(&mut rx)), vec![AdapterStatus::Stubbed]);
    }

    #[test]
    fn remove_device_store_deletes_sqlite_side_files() {
        let dir = std::env::temp_dir().join(format!(
            "thinwire-wa-rm-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        let store = dir.join("device.sqlite");
        for name in [
            "device.sqlite",
            "device.sqlite-wal",
            "device.sqlite-shm",
            "device.sqlite-revoked",
            "other.txt",
        ] {
            std::fs::write(dir.join(name), b"x").expect("write");
        }
        path::remove_device_store(&store).expect("remove");
        assert!(!store.exists());
        assert!(!dir.join("device.sqlite-wal").exists());
        assert!(!dir.join("device.sqlite-shm").exists());
        assert!(
            !path::revoked_marker(&store).exists(),
            "a successful delete clears the revoked mark"
        );
        assert!(dir.join("other.txt").exists());
        path::remove_device_store(&store).expect("missing files are fine");
        let _ = std::fs::remove_dir_all(&dir);
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
