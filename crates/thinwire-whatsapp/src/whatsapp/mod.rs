// SPDX-License-Identifier: AGPL-3.0-only
//! WhatsApp unofficial linked-device adapter (whatsapp-rust).
//!
//! Experimental. ToS / ban risk. Feature `whatsapp-web` may pair only after
//! the UI has accepted the full-screen ban gate. Secrets never travel on
//! commands. A build without this crate uses the MIT stub in
//! `thinwire-protocol` (#77).

mod inbox;
mod link;
mod path;
mod session;

#[cfg(feature = "whatsapp-web")]
mod live;

use std::sync::Arc;

use thinwire_protocol::{
    AccountState, AdapterCommand, AdapterError, AdapterEvent, AdapterStatus, EventTx,
    ProtocolAdapter, ProtocolCapabilities, ProtocolId, SupportClass, WhatsAppPhoneVault,
    emit_chat_list_loaded, emit_history_loaded, emit_older_history_loaded, emit_send_rejected,
    emit_status,
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

/// Longest wait at shutdown for the sends in flight, before the client
/// stops. With the link's own wait it stays under the app close limit, so a
/// message sent just before the window closed still goes out.
const SEND_DRAIN: std::time::Duration = std::time::Duration::from_secs(1);

// The send wait and the link stop fit in the app close limit, with time left
// for `Stopped` to reach the app.
const _: () = assert!(
    SEND_DRAIN.as_millis() + link::SHUTDOWN_WAIT.as_millis()
        < thinwire_protocol::APP_CLOSE_LIMIT.as_millis()
);

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
    /// Sends and retries whose network result has not come yet. Shutdown
    /// waits for them, up to [`SEND_DRAIN`].
    sends: SendsInFlight,
}

/// The send tasks that still run: a task id and the conversation id of
/// each. Never the text.
#[derive(Clone, Default)]
struct SendsInFlight(Arc<tokio::sync::watch::Sender<Running>>);

#[derive(Default)]
struct Running {
    next: u64,
    chats: std::collections::BTreeMap<u64, String>,
}

/// One running send. It leaves the set when the task ends or is dropped. A
/// guard dropped before [`SendGuard::finish`] means the task was cut off
/// (the runtime stopped at exit): that is logged, with the chat id only.
struct SendGuard {
    sends: Arc<tokio::sync::watch::Sender<Running>>,
    id: u64,
    finished: bool,
}

impl SendGuard {
    /// The send has its network result. Call it after the answer went out.
    fn finish(mut self) {
        self.finished = true;
    }
}

impl Drop for SendGuard {
    fn drop(&mut self) {
        let mut chat = None;
        self.sends
            .send_modify(|running| chat = running.chats.remove(&self.id));
        if !self.finished
            && let Some(chat) = chat
        {
            tracing::warn!(chat = %thinwire_protocol::LogChatId(&chat), "whatsapp send abandoned before its result");
        }
    }
}

impl SendsInFlight {
    fn begin(&self, conversation_id: &str) -> SendGuard {
        let mut id = 0;
        self.0.send_modify(|running| {
            id = running.next;
            running.next += 1;
            running.chats.insert(id, conversation_id.to_string());
        });
        SendGuard {
            sends: Arc::clone(&self.0),
            id,
            finished: false,
        }
    }

    fn is_idle(&self) -> bool {
        self.0.borrow().chats.is_empty()
    }

    /// The conversation ids of the sends that still run, oldest first.
    fn chats(&self) -> Vec<String> {
        self.0.borrow().chats.values().cloned().collect()
    }

    /// `true` when every send ended within `limit`.
    async fn wait_idle(&self, limit: std::time::Duration) -> bool {
        let mut running = self.0.subscribe();
        matches!(
            tokio::time::timeout(limit, running.wait_for(|running| running.chats.is_empty())).await,
            Ok(Ok(_))
        )
    }
}

impl WhatsAppAdapter {
    #[must_use]
    pub fn new(phone: Arc<WhatsAppPhoneVault>) -> Self {
        Self {
            risk_acknowledged: false,
            phone,
            session: session::Session::default(),
            link: None,
            sends: SendsInFlight::default(),
        }
    }

    #[must_use]
    pub const fn capabilities() -> ProtocolCapabilities {
        CAPABILITIES
    }

    /// The status line only. No seed rows: the shell drops inbox events of
    /// an account that is not linked (ADR 0010 rule 1).
    fn seed_status(&self, events: &EventTx) {
        emit_status(
            events,
            ProtocolId::WhatsApp,
            AdapterStatus::Stubbed,
            CAPABILITIES.detail,
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
            thinwire_protocol::emit_stopped(events, ProtocolId::WhatsApp);
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
        // The sidebar preview and order come from the chat upsert.
        if let Some(upsert) = upsert {
            let _ = events.send(upsert);
        }
        // SendAccepted or SendRejected comes with the network result.
        let answer = session::SendRequest {
            conversation_id: conversation_id.to_string(),
            request,
            retry: false,
        };
        self.spawn_send(sender, jid, pending, body, answer, events);
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
        // The retained history stays in memory while the link reconnects, so
        // an offline page still answers with real rows and a real `more`.
        // `more == false` never comes from a short drop (Codex r4103931208).
        // After a logout the inbox is empty: an unknown chat ends paging.
        let (page, more) = match inbox::parse_conversation_id(conversation_id) {
            Some(jid) => self
                .session
                .with_inbox(|inbox| inbox.older_page(jid, before, inbox::OPEN_CHAT_MESSAGES)),
            None => (Vec::new(), false),
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
        let answer = session::SendRequest {
            conversation_id: conversation_id.to_string(),
            request,
            retry: true,
        };
        self.spawn_send(sender, jid, message_id.to_string(), body, answer, events);
    }

    fn spawn_send(
        &self,
        (generation, sender): session::LinkSender,
        jid: String,
        pending: String,
        body: String,
        answer: session::SendRequest,
        events: &EventTx,
    ) {
        let session = self.session.clone();
        let events = events.clone();
        let owner = self.link.as_ref().map(|link| link.callbacks(generation));
        // Counted before the spawn, so a shutdown right after sees it.
        let running = self.sends.begin(&answer.conversation_id);
        tokio::spawn(async move {
            let result = sender.send_text(&jid, &body).await;
            let revoked = session.finish_send(generation, &jid, &pending, result, &answer, &events);
            running.finish();
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
        self.seed_status(&events);
    }

    /// Let the sends in flight finish (up to [`SEND_DRAIN`]), stop pairing,
    /// close the linked-device bot and its SQLite session, then `Stopped`.
    /// Without the spike feature nothing runs: `Stopped` at once.
    fn shutdown(&mut self, events: &EventTx) {
        self.risk_acknowledged = false;
        let link = self.link.take();
        if link.is_none() && self.sends.is_idle() {
            // No row comes after `Stopped` (Codex r4139029620).
            self.session.stop_mute_timer();
            thinwire_protocol::emit_stopped(events, ProtocolId::WhatsApp);
            return;
        }
        let events = events.clone();
        let session = self.session.clone();
        let sends = self.sends.clone();
        tokio::spawn(async move {
            // A message sent just before the close still goes out, and its
            // answer comes before `Stopped`.
            if !sends.wait_idle(SEND_DRAIN).await {
                // The client stops now. Each send that still runs is named
                // (chat id, no text). Its result, if any, still answers it.
                for chat in sends.chats() {
                    tracing::warn!(chat = %thinwire_protocol::LogChatId(&chat), "whatsapp send still running at shutdown; stopping the client");
                }
            }
            let stopped = match link {
                Some(link) => link.shutdown().await,
                None => true,
            };
            // After the owner stops, no client event arms a new timer. No row
            // comes after `Stopped` (Codex r4139029620).
            session.stop_mute_timer();
            Self::finish_shutdown(stopped, &events);
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

    use thinwire_protocol::Delivery;

    /// A WhatsApp-looking id with no JID. The adapter never lists it.
    const PLACEHOLDER_ID: &str = "whatsapp:placeholder";

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

    use thinwire_protocol::{AdapterEvent, RedactedPairingSecret};
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    /// Codex r4139029620: shutdown stops a pending mute timer before
    /// `Stopped`. No row comes after `Stopped`, and the timer task lets go
    /// of the session. Both paths: no link, and a running link.
    #[tokio::test]
    async fn shutdown_stops_a_pending_mute_timer() {
        for with_link in [false, true] {
            let (tx, mut rx) = unbounded_channel();
            let mut adapter = WhatsAppAdapter::new(Arc::new(WhatsAppPhoneVault::new()));
            if with_link {
                let (_fake, handle, owner_session, _owner_rx) =
                    link::tests::owner(link::tests::Fake::default());
                handle.begin(None, 1);
                handle.flush().await;
                adapter.session = owner_session;
                adapter.link = Some(handle);
            } else {
                adapter.session.begin(1);
            }
            adapter.session.apply(LinkEvent::Connected, 1, &tx);
            let owners = adapter.session.owners();
            let end = inbox::now_ms() + 100;
            let timed = LinkEvent::Mute {
                jid: CHAT.into(),
                mute: inbox::Mute::UntilMs(end),
            };
            adapter.session.apply(timed, 1, &tx);
            assert!(adapter.session.has_mute_timer());
            assert_eq!(adapter.session.owners(), owners + 1, "the timer task");
            drain(&mut rx);

            adapter.shutdown(&tx);
            let stopped = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .expect("Stopped in time")
                .expect("channel open");
            assert_eq!(
                stopped,
                AdapterEvent::Stopped {
                    protocol: ProtocolId::WhatsApp
                }
            );
            assert!(!adapter.session.has_mute_timer(), "with_link={with_link}");
            // Past the mute end: an aborted timer sends nothing.
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            assert!(inbox::now_ms() > end);
            assert!(rx.try_recv().is_err(), "no event after Stopped");
            assert!(
                adapter.session.owners() <= owners,
                "the timer task let go of the session"
            );
        }
    }

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
                mute: Default::default(),
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
    fn fake_pair_flow_reaches_ready_with_no_early_inbox_event() {
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
        // ADR 0010 rule 1: no inbox event before Account Linked.
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
        assert!(
            !events[..linked]
                .iter()
                .any(|event| event.inbox_protocol().is_some())
        );
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
            rx.recv().await.expect("chat row"),
            AdapterEvent::ConversationUpsert { .. }
        ));
        // #98: SendAccepted comes only after the server accepted it.
        let answer = answer_events(&mut rx).await;
        assert!(accepted(&answer, 1));
        assert!(answer.iter().any(|event| matches!(
            event,
            AdapterEvent::MessageReplaced { old_id, message, .. }
                if old_id == &pending && message.id == "SRV1"
        )));
        assert_eq!(
            sender.sent.lock().expect("lock").as_slice(),
            &[(CHAT.to_string(), "hello".to_string())]
        );
    }

    #[tokio::test]
    async fn fake_send_failure_rejects_the_send_and_a_retry_answers_later() {
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
        let answer = answer_events(&mut rx).await;
        for event in &answer {
            if let AdapterEvent::Status { status, detail, .. } = event {
                assert_eq!(*status, AdapterStatus::Error);
                assert!(!detail.contains("secret body"));
                assert!(!detail.contains("111"));
            }
        }
        // #98: no SendAccepted before the result; the row goes, the text
        // stays in the compose field.
        assert!(!accepted(&answer, 1));
        assert!(rejected(&answer, 1));
        assert!(answer.iter().any(|event| matches!(
            event,
            AdapterEvent::MessagesRemoved { message_ids, .. } if message_ids == &vec![pending.clone()]
        )));

        // A failed row (from an earlier failed retry) retries, and the retry
        // is answered after its result: rejected, and the row stays Failed.
        let failed = adapter.session.with_inbox(|inbox| {
            let (id, _, _) = inbox.begin_send(CHAT, "old", 1);
            let _ = inbox.fail_send(CHAT, &id);
            id
        });
        adapter
            .handle(
                AdapterCommand::ResendMessage {
                    protocol: ProtocolId::WhatsApp,
                    conversation_id: "whatsapp:111@s.whatsapp.net".into(),
                    message_id: failed.clone(),
                    request: 2,
                },
                &tx,
            )
            .expect("resend");
        let answer = answer_events(&mut rx).await;
        assert!(matches!(
            answer.first(),
            Some(AdapterEvent::MessageDelivery {
                delivery: Delivery::Pending,
                ..
            })
        ));
        assert!(!accepted(&answer, 2));
        assert!(rejected(&answer, 2));
        assert!(answer.iter().any(|event| matches!(
            event,
            AdapterEvent::MessageDelivery { message_id, delivery: Delivery::Failed, .. }
                if message_id == &failed
        )));

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

    /// #98: a retry that the server accepts answers SendAccepted with the
    /// replaced row.
    #[tokio::test]
    async fn an_accepted_retry_answers_after_the_server() {
        let (mut adapter, mut rx, tx) = connected(Arc::new(FakeSender::default()));
        let failed = adapter.session.with_inbox(|inbox| {
            let (id, _, _) = inbox.begin_send(CHAT, "old", 1);
            let _ = inbox.fail_send(CHAT, &id);
            id
        });
        adapter
            .handle(
                AdapterCommand::ResendMessage {
                    protocol: ProtocolId::WhatsApp,
                    conversation_id: "whatsapp:111@s.whatsapp.net".into(),
                    message_id: failed.clone(),
                    request: 4,
                },
                &tx,
            )
            .expect("resend");
        let answer = answer_events(&mut rx).await;
        assert!(accepted(&answer, 4));
        assert!(answer.iter().any(|event| matches!(
            event,
            AdapterEvent::MessageReplaced { old_id, .. } if old_id == &failed
        )));
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

    /// The pending row and its chat row. The answer comes with the network
    /// result (#98).
    async fn expect_pending(rx: &mut UnboundedReceiver<AdapterEvent>) -> String {
        let pending = match rx.recv().await.expect("pending row") {
            AdapterEvent::MessageReceived { message } => message.id,
            other => panic!("unexpected {other:?}"),
        };
        assert!(matches!(
            rx.recv().await.expect("chat row"),
            AdapterEvent::ConversationUpsert { .. }
        ));
        pending
    }

    /// Every event up to the answer of a send or retry, and the events that
    /// came with it.
    async fn answer_events(rx: &mut UnboundedReceiver<AdapterEvent>) -> Vec<AdapterEvent> {
        let mut events = Vec::new();
        loop {
            let event = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .expect("an answer in time")
                .expect("open channel");
            let answer = matches!(
                event,
                AdapterEvent::SendAccepted { .. } | AdapterEvent::SendRejected { .. }
            );
            events.push(event);
            if answer {
                break;
            }
        }
        events.extend(drain(rx));
        events
    }

    fn accepted(events: &[AdapterEvent], request: u64) -> bool {
        events.iter().any(|event| {
            matches!(event, AdapterEvent::SendAccepted { request: seen, .. } if *seen == request)
        })
    }

    fn rejected(events: &[AdapterEvent], request: u64) -> bool {
        events.iter().any(|event| {
            matches!(event, AdapterEvent::SendRejected { request: seen, .. } if *seen == request)
        })
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
    async fn network_error_on_send_rejects_it_and_keeps_the_link() {
        let (mut adapter, mut rx, tx) = connected(failing(session::SendFailure::Network));
        adapter.handle(send_hi(), &tx).expect("queued");
        let pending = expect_pending(&mut rx).await;
        let answer = answer_events(&mut rx).await;
        assert!(answer.iter().any(|event| matches!(
            event,
            AdapterEvent::Status { status: AdapterStatus::Error, detail, .. } if detail == session::SEND_NETWORK
        )));
        // #98: a failed new send loses its row; the text stays in compose.
        assert!(answer.iter().any(|event| matches!(
            event,
            AdapterEvent::MessagesRemoved { message_ids, .. } if message_ids == &vec![pending.clone()]
        )));
        assert!(rejected(&answer, 1));
        assert!(!accepted(&answer, 1));
        assert!(adapter.session.is_connected());
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
        let _pending = expect_pending(&mut rx).await;
        let answer = answer_events(&mut rx).await;
        assert!(accepted(&answer, 1));
        assert!(
            answer
                .iter()
                .any(|event| matches!(event, AdapterEvent::MessageReplaced { .. }))
        );
    }

    #[tokio::test]
    async fn revoked_device_on_send_clears_the_inbox() {
        let (mut adapter, mut rx, tx) = connected(failing(session::SendFailure::Unlinked));
        adapter.handle(send_hi(), &tx).expect("queued");
        let _pending = expect_pending(&mut rx).await;
        let answer = answer_events(&mut rx).await;
        assert!(answer.iter().any(|event| matches!(
            event,
            AdapterEvent::Status { status: AdapterStatus::Error, detail, .. } if detail == session::SEND_UNLINKED
        )));
        assert!(rejected(&answer, 1));
        assert!(answer.iter().any(|event| matches!(
            event,
            AdapterEvent::ConversationRemoved { id, .. } if id == "whatsapp:111@s.whatsapp.net"
        )));
        assert!(answer.iter().any(|event| matches!(
            event,
            AdapterEvent::Account {
                state: AccountState::Unlinked,
                ..
            }
        )));
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
        let _pending = expect_pending(&mut rx).await;
        let answer = answer_events(&mut rx).await;
        assert!(accepted(&answer, 1));
        assert!(
            answer
                .iter()
                .any(|event| matches!(event, AdapterEvent::MessageReplaced { .. }))
        );
        assert_eq!(current.sent.lock().expect("lock").len(), 1);
        assert!(stale.sent.lock().expect("lock").is_empty());
    }

    /// Codex r4093192847: after a cancel or a new pairing, events of the old
    /// link must not mark it Ready or refill the cleared inbox.
    #[test]
    fn stale_link_events_are_dropped_after_cancel_or_replace() {
        let (tx, mut rx) = unbounded_channel();
        let adapter = with_sender(Arc::new(FakeSender::default()));
        adapter.session.apply(LinkEvent::Connected, 1, &tx);
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

    /// A message still sending when the app closes is not cut off: shutdown
    /// waits for its result, and the answer comes before `Stopped`.
    #[tokio::test]
    async fn shutdown_waits_for_a_send_in_flight() {
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
        let _pending = expect_pending(&mut rx).await;

        adapter.shutdown(&tx);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !drain(&mut rx)
                .iter()
                .any(|event| matches!(event, AdapterEvent::Stopped { .. })),
            "no Stopped while the send runs"
        );

        gated.release.notify_one();
        let mut seen = Vec::new();
        while !seen
            .iter()
            .any(|event| matches!(event, AdapterEvent::Stopped { .. }))
        {
            let event = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .expect("Stopped in time")
                .expect("open channel");
            seen.push(event);
        }
        let answer = seen
            .iter()
            .position(|event| matches!(event, AdapterEvent::SendRejected { request: 1, .. }))
            .expect("the send got its answer");
        let stopped = seen
            .iter()
            .position(|event| matches!(event, AdapterEvent::Stopped { .. }))
            .expect("Stopped");
        assert!(
            answer < stopped,
            "the answer comes before Stopped: {seen:?}"
        );
    }

    /// Log lines of this thread while the guard lives. Plain text, no ANSI.
    fn capture_logs() -> (
        std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
        tracing::subscriber::DefaultGuard,
    ) {
        #[derive(Clone)]
        struct Buf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Buf {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let bytes = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let writer = Buf(std::sync::Arc::clone(&bytes));
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .with_writer(move || writer.clone())
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        fresh_interest();
        (bytes, guard)
    }

    /// Another test thread may register a callsite at the same time as the
    /// capture starts, and cache "no subscriber wants it". Read every
    /// callsite's interest again right before the step that logs.
    fn fresh_interest() {
        tracing::callsite::rebuild_interest_cache();
    }

    fn logged(bytes: &std::sync::Mutex<Vec<u8>>) -> String {
        String::from_utf8_lossy(
            &bytes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
        .into_owned()
    }

    /// A send that never ends holds the shutdown only for [`SEND_DRAIN`].
    /// #238: the send cut off at shutdown is logged with its chat id, never
    /// its text.
    #[tokio::test]
    async fn shutdown_stops_waiting_for_a_hung_send() {
        let (logs, guard) = capture_logs();
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
        let _pending = expect_pending(&mut rx).await;

        let start = std::time::Instant::now();
        fresh_interest();
        adapter.shutdown(&tx);
        let stopped = loop {
            let event = tokio::time::timeout(SEND_DRAIN * 2, rx.recv())
                .await
                .expect("Stopped in time")
                .expect("open channel");
            if matches!(event, AdapterEvent::Stopped { .. }) {
                break std::time::Instant::now();
            }
        };
        let waited = stopped - start;
        assert!(waited >= SEND_DRAIN, "{waited:?}");
        assert!(waited < SEND_DRAIN * 2, "{waited:?}");
        let log = logged(&logs);
        let line = log
            .lines()
            .find(|line| line.contains("whatsapp send still running at shutdown"))
            .unwrap_or_else(|| panic!("no log line: {log}"));
        assert!(line.contains("WARN"), "{line}");
        let shown = thinwire_protocol::LogChatId("whatsapp:111@s.whatsapp.net").to_string();
        assert!(line.contains(&shown), "{line}");
        drop(guard);
    }

    /// #238: a send task dropped before its result (the runtime stops at
    /// exit) is logged with its redacted chat id. A finished send is not.
    /// A 1:1 chat id holds the contact's phone number; the log line keeps
    /// only its last four digits.
    #[test]
    fn a_send_cut_off_before_its_result_is_logged() {
        const PHONE: &str = "4915550100";
        let chat = format!("whatsapp:{PHONE}@s.whatsapp.net");
        let (logs, _guard) = capture_logs();
        let sends = SendsInFlight::default();
        let running = sends.begin(&chat);
        assert!(!sends.is_idle());
        assert_eq!(sends.chats(), vec![chat.clone()]);
        fresh_interest();
        drop(running);
        let log = logged(&logs);
        let line = log
            .lines()
            .find(|line| line.contains("whatsapp send abandoned before its result"))
            .unwrap_or_else(|| panic!("no log line: {log}"));
        let shown = thinwire_protocol::LogChatId(&chat).to_string();
        assert_eq!(shown, "whatsapp:…0100@s.whatsapp.net");
        assert!(line.contains("WARN") && line.contains(&shown), "{line}");
        assert!(!log.contains(PHONE), "never the full phone number: {log}");
        let finished = sends.begin("whatsapp:333@s.whatsapp.net");
        finished.finish();
        assert!(
            !logged(&logs).contains("whatsapp:333"),
            "a finished send is not logged"
        );
        assert!(sends.is_idle());
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
        let _pending = expect_pending(&mut rx).await;

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
        // Rule 4: the lost request still gets its answer, and nothing else
        // of the old link comes out.
        let late = drain(&mut rx);
        assert!(
            late.len() == 1 && rejected(&late, 1),
            "old send result leaked: {late:?}"
        );
        assert!(adapter.session.is_connected());

        adapter.handle(send_hi(), &tx).expect("send on link 2");
        let _pending = expect_pending(&mut rx).await;
        let answer = answer_events(&mut rx).await;
        assert!(accepted(&answer, 1));
        assert!(
            answer
                .iter()
                .any(|event| matches!(event, AdapterEvent::MessageReplaced { .. }))
        );
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
                    mute: Default::default(),
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

        // An unknown id: no rows, no error, `more == false`.
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
        // Codex r4103931208: a short drop does not end paging. The retained
        // rows still page, with the real `more`.
        adapter.session.apply(LinkEvent::Disconnected, 1, &tx);
        drain(&mut rx);
        older(&mut adapter, "m070");
        let events = drain(&mut rx);
        let rows = events
            .iter()
            .filter(|event| matches!(event, AdapterEvent::MessageReceived { .. }))
            .count();
        assert_eq!(rows, inbox::OPEN_CHAT_MESSAGES);
        assert!(matches!(
            events.last(),
            Some(AdapterEvent::OlderHistoryLoaded { more: true, .. })
        ));
        assert!(statuses(&events).is_empty());

        // After a logout the inbox is empty: paging ends.
        adapter.session.apply(LinkEvent::Connected, 1, &tx);
        adapter.session.apply(LinkEvent::LoggedOut, 1, &tx);
        drain(&mut rx);
        older(&mut adapter, "m070");
        assert!(matches!(
            drain(&mut rx).last(),
            Some(AdapterEvent::OlderHistoryLoaded { more: false, .. })
        ));
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
                mute: Default::default(),
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

    /// ADR 0010 rule 1: history before `Connected` stays in the inbox and
    /// goes out after Linked, with the chat page.
    #[test]
    fn history_before_connect_waits_for_linked() {
        let (tx, mut rx) = unbounded_channel();
        let adapter = with_sender(Arc::new(FakeSender::default()));
        adapter.session.apply(history(), 1, &tx);
        adapter.session.apply(
            LinkEvent::Messages(vec![message(CHAT, "early", "early", 11)]),
            1,
            &tx,
        );
        assert!(drain(&mut rx).is_empty(), "no inbox event before Linked");
        adapter.session.apply(LinkEvent::Connected, 1, &tx);
        let events = drain(&mut rx);
        assert!(matches!(
            events.iter().find(|event| event.inbox_protocol().is_some()
                || matches!(event, AdapterEvent::Account { .. })),
            Some(AdapterEvent::Account {
                state: AccountState::Linked,
                ..
            })
        ));
        assert!(events.iter().any(|event| matches!(
            event,
            AdapterEvent::ConversationUpsert { conversation } if conversation.preview == "early"
        )));
        // A cancel before Linked removes nothing the shell never saw.
        let adapter = with_sender(Arc::new(FakeSender::default()));
        adapter.session.apply(history(), 1, &tx);
        assert!(adapter.session.reset().is_empty());
    }

    /// ADR 0010 contract kit (#109): the WhatsApp adapter with a fake
    /// sender, linked through its session, passes every kit check.
    #[tokio::test]
    async fn contract_kit_passes() {
        let adapter = with_sender(Arc::new(FakeSender::default()));
        let session = adapter.session.clone();
        // The test build compiles WhatsApp without `whatsapp-web`, so check
        // against the capabilities of the feature build.
        let mut kit = thinwire_protocol::contract::Contract::new(Box::new(adapter))
            .with_capabilities(ProtocolCapabilities {
                sends_text: true,
                pages_history: true,
                ..CAPABILITIES
            });
        let events = kit.events();
        session.apply(history(), 1, &events);
        session.apply(LinkEvent::Connected, 1, &events);
        kit.linked().await;
        kit.run_all().await;
    }

    /// #201 qa B: only a phone logout ends the account for good and sends
    /// `AccountEnded`, so the shell drops the WhatsApp mutes. A ban, a dead
    /// pairing, a reconnect, a cancel, and a stop do not.
    #[test]
    fn only_a_logout_ends_the_account() {
        let ended = |events: &[AdapterEvent]| {
            events
                .iter()
                .filter(|event| {
                    matches!(
                        event,
                        AdapterEvent::AccountEnded {
                            protocol: ProtocolId::WhatsApp
                        }
                    )
                })
                .count()
        };
        for event in [
            LinkEvent::LoggedOut,
            LinkEvent::TemporaryBan,
            LinkEvent::PairFailed,
            LinkEvent::PairThrottled,
            LinkEvent::QrExhausted,
            LinkEvent::Disconnected,
        ] {
            let (adapter, mut rx, tx) = connected(Arc::new(FakeSender::default()));
            let expected = usize::from(event == LinkEvent::LoggedOut);
            adapter.session.apply(event.clone(), 1, &tx);
            assert_eq!(ended(&drain(&mut rx)), expected, "{event:?}");
        }

        let (mut adapter, mut rx, tx) = connected(Arc::new(FakeSender::default()));
        adapter
            .handle(AdapterCommand::WhatsAppCancelLink, &tx)
            .expect("cancel");
        adapter.shutdown(&tx);
        assert_eq!(ended(&drain(&mut rx)), 0, "cancel and stop");
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
