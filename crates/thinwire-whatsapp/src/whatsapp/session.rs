// SPDX-License-Identifier: AGPL-3.0-only
//! Shared WhatsApp session state between the adapter and the live client.
//!
//! The live client (feature `whatsapp-web`) turns whatsapp-rust events into
//! [`LinkEvent`] values. Tests feed the same values from fakes. Pairing
//! material travels only inside [`RedactedPairingSecret`].

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use super::inbox::{self, HistoryChat, Inbox, Mute, WaMessage};
use thinwire_protocol::{
    AccountState, AdapterEvent, AdapterStatus, EventTx, ProtocolId, RedactedPairingSecret,
    emit_status,
};

pub(super) const PAIRED: &str =
    "WhatsApp device linked. Connecting. Experimental. Ban risk. Not a supported messenger.";
pub(super) const CONNECTED: &str =
    "Experimental WhatsApp linked device is connected. Ban risk. Not a supported messenger.";
pub(super) const DISCONNECTED: &str =
    "Experimental WhatsApp connection dropped. The client tries to connect again.";
pub(super) const LOGGED_OUT: &str =
    "WhatsApp unlinked this device. Open the ban gate again to pair a new session.";
pub(super) const TEMPORARY_BAN: &str =
    "WhatsApp put a temporary ban on this account. Stop using the experimental client.";
pub(super) const PAIR_FAILED: &str = "WhatsApp pairing failed. Cancel, then try again.";
pub(super) const PAIR_THROTTLED: &str = "WhatsApp limited pair-code requests. Wait some minutes, then cancel and try again. Repeated tries raise the ban risk.";
pub(super) const SEND_NETWORK: &str = "WhatsApp send failed: no connection. Nothing was sent. Try again when the status is connected.";
pub(super) const SEND_UNLINKED: &str = "WhatsApp unlinked this device. Nothing was sent. Open the ban gate again to pair a new session.";
pub(super) const SEND_REJECTED: &str = "WhatsApp did not accept the message. Nothing was sent.";
pub(super) const QR_EXHAUSTED: &str =
    "WhatsApp QR codes expired. Cancel, then start pairing again.";

/// Boxed send future. Resolves to the server message id.
pub(super) type SendFuture<'a> =
    Pin<Box<dyn Future<Output = Result<String, SendFailure>> + Send + 'a>>;

/// Send failure class without server text, JIDs, or bodies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(any(test, feature = "whatsapp-web")), allow(dead_code))]
pub(super) enum SendFailure {
    /// Transport or IQ failure. The client reconnects on its own.
    Network,
    /// The phone revoked this linked device.
    Unlinked,
    /// The server or the client refused the message.
    Rejected,
}

impl SendFailure {
    #[must_use]
    pub(super) const fn detail(self) -> &'static str {
        match self {
            Self::Network => SEND_NETWORK,
            Self::Unlinked => SEND_UNLINKED,
            Self::Rejected => SEND_REJECTED,
        }
    }
}

/// Outgoing text transport. The live client wraps whatsapp-rust. Tests use a fake.
pub(super) trait WhatsAppSender: Send + Sync {
    fn send_text<'a>(&'a self, chat_jid: &'a str, body: &'a str) -> SendFuture<'a>;
}

/// What the linked-device client reports to the adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(feature = "whatsapp-web"), allow(dead_code))]
pub(super) enum LinkEvent {
    Qr(RedactedPairingSecret),
    PairCode(RedactedPairingSecret),
    PairFailed,
    /// The server refused a pair-code request as rate limited.
    PairThrottled,
    QrExhausted,
    Paired,
    Connected,
    Disconnected,
    LoggedOut,
    TemporaryBan,
    History {
        chats: Vec<HistoryChat>,
        push_names: Vec<(String, String)>,
    },
    Messages(Vec<WaMessage>),
    /// The phone muted or unmuted a chat (app-state `MuteAction`).
    Mute {
        jid: String,
        mute: Mute,
    },
}

/// A connected sender and the link generation it belongs to.
pub(super) type LinkSender = (u64, Arc<dyn WhatsAppSender>);

/// Session generation while no link runs. Link tokens start at 1.
const NO_LINK: u64 = 0;

#[derive(Default)]
struct State {
    inbox: Inbox,
    connected: bool,
    sender: Option<Arc<dyn WhatsAppSender>>,
    /// Link generation that may install a sender. Set by [`Session::begin`].
    generation: u64,
    /// The shell's pairing id. QR codes and pair codes carry it.
    pairing: u64,
    /// Status text of the event that stopped the link. Cleared on a new link.
    stopped: Option<&'static str>,
    /// This link sent `Account { Linked }`. Until then the shell drops inbox
    /// events (ADR 0010 rule 1), so the session keeps them in the inbox and
    /// sends the chat page on `Connected`.
    announced: bool,
    /// The timer for the next timed mute end (Unix ms), and its task. One
    /// timer runs at a time (#168 item 13).
    mute_timer: Option<(i64, tokio::task::JoinHandle<()>)>,
}

impl LinkEvent {
    /// The client must stop: no reconnect after a logout, a ban, or a dead pairing.
    #[must_use]
    #[cfg_attr(not(any(test, feature = "whatsapp-web")), allow(dead_code))]
    pub(super) const fn stops_link(&self) -> bool {
        matches!(
            self,
            Self::LoggedOut
                | Self::TemporaryBan
                | Self::PairFailed
                | Self::PairThrottled
                | Self::QrExhausted
        )
    }

    /// The phone revoked this device. The saved device store is no longer valid.
    #[must_use]
    #[cfg_attr(not(any(test, feature = "whatsapp-web")), allow(dead_code))]
    pub(super) const fn invalidates_device(&self) -> bool {
        matches!(self, Self::LoggedOut)
    }

    const fn stop_detail(&self) -> Option<&'static str> {
        Some(match self {
            Self::LoggedOut => LOGGED_OUT,
            Self::TemporaryBan => TEMPORARY_BAN,
            Self::PairFailed => PAIR_FAILED,
            Self::PairThrottled => PAIR_THROTTLED,
            Self::QrExhausted => QR_EXHAUSTED,
            _ => return None,
        })
    }
}

/// Inbox plus link state. Cheap to clone; all clones share one state.
#[derive(Clone, Default)]
pub(super) struct Session {
    state: Arc<Mutex<State>>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session").finish_non_exhaustive()
    }
}

impl Session {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[cfg_attr(not(any(test, feature = "whatsapp-web")), allow(dead_code))]
    /// Install the sender of link `generation`. A start from an older
    /// generation can finish late; its sender is ignored, so it cannot
    /// replace the sender of the current link. Returns `true` if stored.
    pub(super) fn attach_sender(&self, generation: u64, sender: Arc<dyn WhatsAppSender>) -> bool {
        let mut state = self.lock();
        if state.generation != generation {
            return false;
        }
        state.sender = Some(sender);
        true
    }

    #[must_use]
    pub(super) fn is_connected(&self) -> bool {
        self.lock().connected
    }

    /// Connected sender and its link generation, or `None` while pairing or
    /// offline. Pass the generation to [`Self::finish_send`].
    #[must_use]
    pub(super) fn sender(&self) -> Option<LinkSender> {
        let state = self.lock();
        if state.connected {
            state
                .sender
                .clone()
                .map(|sender| (state.generation, sender))
        } else {
            None
        }
    }

    /// Stop the link and forget the inbox. Returns removals for the UI.
    ///
    /// Events and send results of the old link are stale from here on.
    pub(super) fn reset(&self) -> Vec<AdapterEvent> {
        Self::reset_state(&mut self.lock())
    }

    fn reset_state(state: &mut State) -> Vec<AdapterEvent> {
        Self::stop_mute_timer(state);
        state.connected = false;
        state.sender = None;
        state.stopped = None;
        state.generation = NO_LINK;
        let removed = state.inbox.clear();
        // Rows the shell never saw need no removal.
        if std::mem::take(&mut state.announced) {
            removed
        } else {
            Vec::new()
        }
    }

    /// Status text of the event that stopped the last link, if any.
    #[must_use]
    pub(super) fn stopped(&self) -> Option<&'static str> {
        self.lock().stopped
    }

    /// A new link starts. Forget the last stop reason and the old sender.
    #[cfg_attr(not(feature = "whatsapp-web"), allow(dead_code))]
    pub(super) fn begin(&self, generation: u64) {
        let mut state = self.lock();
        state.stopped = None;
        state.generation = generation;
        state.sender = None;
        state.announced = false;
        // A timer of the old link has a stale generation: it would never end
        // a mute of this link (Codex r4138502157).
        Self::stop_mute_timer(&mut state);
        state.inbox.new_connection();
    }

    fn stop_mute_timer(state: &mut State) {
        if let Some((_, task)) = state.mute_timer.take() {
            task.abort();
        }
    }

    /// The session still belongs to link `generation`: no cancel reset it
    /// and no newer link began.
    #[must_use]
    pub(super) fn is_link(&self, generation: u64) -> bool {
        let state = self.lock();
        state.generation != NO_LINK && state.generation == generation
    }

    /// The shell's id for the pairing that runs now (`WhatsAppBeginLink`).
    #[cfg_attr(not(any(test, feature = "whatsapp-web")), allow(dead_code))]
    pub(super) fn set_pairing(&self, pairing: u64) {
        self.lock().pairing = pairing;
    }

    pub(super) fn with_inbox<T>(&self, action: impl FnOnce(&mut Inbox) -> T) -> T {
        action(&mut self.lock().inbox)
    }

    /// Apply one client event of link `generation` and push the UI events.
    ///
    /// The generation check, the state change, and the event sends happen
    /// under one lock. A callback of a canceled or replaced link cannot pass
    /// the check and then apply after a reset.
    #[cfg_attr(not(any(test, feature = "whatsapp-web")), allow(dead_code))]
    pub(super) fn apply(&self, event: LinkEvent, generation: u64, events: &EventTx) {
        let mut state = self.lock();
        if state.generation == NO_LINK || state.generation != generation {
            return;
        }
        let stop = event.stop_detail();
        let mute_may_change = matches!(event, LinkEvent::History { .. } | LinkEvent::Mute { .. });
        let out = match event {
            LinkEvent::Qr(code) => vec![AdapterEvent::WhatsAppQr {
                code,
                generation: state.pairing,
            }],
            LinkEvent::PairCode(code) => vec![AdapterEvent::WhatsAppPairCode {
                code,
                generation: state.pairing,
            }],
            LinkEvent::PairFailed => {
                status(events, AdapterStatus::Error, PAIR_FAILED);
                Vec::new()
            }
            LinkEvent::PairThrottled => {
                status(events, AdapterStatus::Error, PAIR_THROTTLED);
                Vec::new()
            }
            LinkEvent::QrExhausted => {
                status(events, AdapterStatus::Error, QR_EXHAUSTED);
                Vec::new()
            }
            LinkEvent::Paired => {
                status(events, AdapterStatus::Connecting, PAIRED);
                Vec::new()
            }
            LinkEvent::Connected => {
                state.connected = true;
                state.announced = true;
                status(events, AdapterStatus::Ready, CONNECTED);
                // ADR 0010 rule 1: Linked comes before the first inbox event.
                let mut out = vec![account(AccountState::Linked)];
                out.extend(state.inbox.chat_page());
                out
            }
            LinkEvent::Disconnected => {
                state.connected = false;
                // The next connection starts here: a reconnect can send
                // History before Connected (Codex r4138502149).
                state.inbox.new_connection();
                status(events, AdapterStatus::Connecting, DISCONNECTED);
                // A reconnect: the shell keeps the session and its rows.
                vec![account(AccountState::Linking)]
            }
            LinkEvent::LoggedOut => {
                let removed = Self::reset_state(&mut state);
                status(events, AdapterStatus::Error, LOGGED_OUT);
                removed
            }
            LinkEvent::TemporaryBan => {
                state.connected = false;
                status(events, AdapterStatus::Error, TEMPORARY_BAN);
                Vec::new()
            }
            // Before Linked the rows stay in the inbox only (rule 1).
            LinkEvent::History { chats, push_names } => {
                let rows = state.inbox.apply_history(chats, push_names);
                if state.announced { rows } else { Vec::new() }
            }
            LinkEvent::Messages(messages) => {
                let rows = state.inbox.apply_messages(messages);
                if state.announced { rows } else { Vec::new() }
            }
            LinkEvent::Mute { jid, mute } => {
                let row = state.inbox.apply_mute(&jid, mute);
                if state.announced {
                    row.into_iter().collect()
                } else {
                    Vec::new()
                }
            }
        };
        let mut out = out;
        if let Some(detail) = stop {
            state.connected = false;
            state.stopped = Some(detail);
            // Logout, ban, or a dead pairing ends the session.
            out.push(account(AccountState::Unlinked));
        }
        for event in out {
            let _ = events.send(event);
        }
        if mute_may_change {
            self.arm_mute_timer(&mut state, generation, events);
        }
    }

    /// Wake at the next timed mute end, so the row stops being muted then
    /// and not only at the next message (#168 item 13). An earlier end
    /// replaces the timer.
    fn arm_mute_timer(&self, state: &mut State, generation: u64, events: &EventTx) {
        let Some(end) = state.inbox.next_mute_end() else {
            return;
        };
        if let Some((armed, task)) = &state.mute_timer {
            if *armed <= end && !task.is_finished() {
                return;
            }
            task.abort();
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let wait = u64::try_from(end.saturating_sub(inbox::now_ms())).unwrap_or(0);
        let session = self.clone();
        let events = events.clone();
        let task = runtime.spawn(async move {
            // One millisecond more: the sleep clock and the wall clock can
            // differ. A timer that wakes too early arms again.
            tokio::time::sleep(Duration::from_millis(wait.saturating_add(1))).await;
            session.end_mutes(generation, &events);
        });
        state.mute_timer = Some((end, task));
    }

    /// The mute timer of link `generation` fired: end the mutes that passed.
    fn end_mutes(&self, generation: u64, events: &EventTx) {
        let mut state = self.lock();
        if state.generation == NO_LINK || state.generation != generation {
            return;
        }
        state.mute_timer = None;
        let rows = state.inbox.expire_mutes(inbox::now_ms());
        if state.announced {
            for row in rows {
                let _ = events.send(row);
            }
        }
        self.arm_mute_timer(&mut state, generation, events);
    }

    /// Apply the result of a send that started on link `generation`, and
    /// answer the shell's request only now, after the network result
    /// (qaproto, #98): `SendAccepted` when the server accepted the message,
    /// else `SendRejected`. A failed new send loses its row (the text stays
    /// in the compose field); a failed retry keeps its row as Failed.
    ///
    /// A result from an older link is dropped: that link's inbox and sender
    /// are gone, and an `Unlinked` result must not reset the new link.
    ///
    /// Returns `true` if the phone revoked the current link. The caller must
    /// tell the link owner, so it stops the client and deletes the store.
    #[must_use]
    pub(super) fn finish_send(
        &self,
        generation: u64,
        jid: &str,
        pending: &str,
        result: Result<String, SendFailure>,
        answer: &SendRequest,
        events: &EventTx,
    ) -> bool {
        let mut state = self.lock();
        if state.generation != generation {
            // The link that sent it is gone, and its rows with it. Rule 4:
            // answer also a request that a reconnect lost.
            let _ = events.send(answer.rejected());
            return false;
        }
        let revoked = result == Err(SendFailure::Unlinked);
        let out = match result {
            Ok(server_id) => {
                let mut out = vec![answer.accepted()];
                out.extend(state.inbox.confirm_send(jid, pending, server_id));
                out
            }
            Err(failure) => {
                status(events, AdapterStatus::Error, failure.detail());
                let mut out = if answer.retry {
                    state.inbox.fail_send(jid, pending)
                } else {
                    state.inbox.drop_send(jid, pending)
                };
                out.push(answer.rejected());
                if failure == SendFailure::Unlinked {
                    out.extend(Self::reset_state(&mut state));
                    out.push(account(AccountState::Unlinked));
                }
                out
            }
        };
        for event in out {
            let _ = events.send(event);
        }
        revoked
    }
}

/// The shell's request of one send or retry (ADR 0010 rule 4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SendRequest {
    pub conversation_id: String,
    pub request: u64,
    /// A `ResendMessage` of a failed row, not a new `SendText`.
    pub retry: bool,
}

impl SendRequest {
    fn accepted(&self) -> AdapterEvent {
        AdapterEvent::SendAccepted {
            protocol: ProtocolId::WhatsApp,
            conversation_id: self.conversation_id.clone(),
            request: self.request,
        }
    }

    fn rejected(&self) -> AdapterEvent {
        AdapterEvent::SendRejected {
            protocol: ProtocolId::WhatsApp,
            conversation_id: self.conversation_id.clone(),
            request: self.request,
        }
    }
}

/// The link state of the WhatsApp account. Only this event changes it in the
/// shell (ADR 0010 rule 1); a `Status` never does.
pub(super) const fn account(state: AccountState) -> AdapterEvent {
    AdapterEvent::Account {
        protocol: ProtocolId::WhatsApp,
        state,
    }
}

fn status(events: &EventTx, status: AdapterStatus, detail: &str) {
    emit_status(events, ProtocolId::WhatsApp, status, detail);
}

#[cfg(test)]
pub(super) mod fake {
    use std::sync::Mutex;

    use super::{SendFailure, SendFuture, WhatsAppSender};

    /// Records sends. Returns `SRV<n>` ids, or the failure in `fail`.
    #[derive(Default)]
    pub(in crate::whatsapp) struct FakeSender {
        pub sent: Mutex<Vec<(String, String)>>,
        pub fail: Option<SendFailure>,
    }

    impl WhatsAppSender for FakeSender {
        fn send_text<'a>(&'a self, chat_jid: &'a str, body: &'a str) -> SendFuture<'a> {
            Box::pin(async move {
                let mut sent = self.sent.lock().expect("fake lock");
                sent.push((chat_jid.to_string(), body.to_string()));
                if let Some(failure) = self.fail {
                    Err(failure)
                } else {
                    Ok(format!("SRV{}", sent.len()))
                }
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    use super::*;
    use crate::whatsapp::inbox::tests::message;
    use thinwire_protocol::Conversation;

    const CHAT: &str = "111@s.whatsapp.net";

    fn linked() -> (Session, EventTx, UnboundedReceiver<AdapterEvent>) {
        let session = Session::default();
        session.begin(1);
        let (tx, rx) = unbounded_channel();
        session.apply(LinkEvent::Connected, 1, &tx);
        (session, tx, rx)
    }

    fn history(mute: Mute) -> LinkEvent {
        LinkEvent::History {
            chats: vec![HistoryChat {
                jid: CHAT.into(),
                name: None,
                unread: 0,
                timestamp: 10,
                messages: Vec::new(),
                mute,
            }],
            push_names: Vec::new(),
        }
    }

    fn last_row(rx: &mut UnboundedReceiver<AdapterEvent>) -> Option<Conversation> {
        let mut row = None;
        while let Ok(event) = rx.try_recv() {
            if let AdapterEvent::ConversationUpsert { conversation } = event {
                row = Some(conversation);
            }
        }
        row
    }

    fn armed_end(session: &Session) -> Option<i64> {
        session.lock().mute_timer.as_ref().map(|(end, _)| *end)
    }

    /// #168 item 12: the session tells the inbox about each connection. A
    /// history value after a reconnect refreshes a mute that a live change
    /// of the last connection set.
    #[test]
    fn a_reconnect_lets_history_refresh_a_live_mute() {
        let (session, tx, mut rx) = linked();
        let live = LinkEvent::Mute {
            jid: CHAT.into(),
            mute: Mute::Forever,
        };
        session.apply(live, 1, &tx);
        session.apply(history(Mute::Off), 1, &tx);
        assert!(
            last_row(&mut rx).expect("row").muted,
            "the live change wins"
        );

        session.apply(LinkEvent::Disconnected, 1, &tx);
        session.apply(LinkEvent::Connected, 1, &tx);
        session.apply(history(Mute::Off), 1, &tx);
        assert!(
            !last_row(&mut rx).expect("row").muted,
            "unmuted while offline"
        );
    }

    /// Codex r4138502149: a reconnect can send History before Connected.
    /// That history value still applies over a live change of the last
    /// connection.
    #[test]
    fn reconnect_history_before_connected_applies_its_mute() {
        let (session, tx, mut rx) = linked();
        let live = LinkEvent::Mute {
            jid: CHAT.into(),
            mute: Mute::Forever,
        };
        session.apply(live, 1, &tx);
        session.apply(history(Mute::Off), 1, &tx);
        assert!(
            last_row(&mut rx).expect("row").muted,
            "the live change wins"
        );

        session.apply(LinkEvent::Disconnected, 1, &tx);
        session.apply(history(Mute::Off), 1, &tx);
        assert!(!last_row(&mut rx).expect("row").muted, "before Connected");
        session.apply(LinkEvent::Connected, 1, &tx);
        assert!(
            !last_row(&mut rx).expect("row").muted,
            "the chat page agrees"
        );
    }

    /// Codex r4138502149: a new link is a new connection too. Its history
    /// before Connected applies over a live change of the old link.
    #[test]
    fn a_new_link_lets_history_refresh_a_live_mute() {
        let (session, tx, mut rx) = linked();
        let live = LinkEvent::Mute {
            jid: CHAT.into(),
            mute: Mute::Forever,
        };
        session.apply(live, 1, &tx);
        assert!(last_row(&mut rx).expect("row").muted);

        session.begin(2);
        session.apply(history(Mute::Off), 2, &tx);
        session.apply(LinkEvent::Connected, 2, &tx);
        assert!(!last_row(&mut rx).expect("row").muted);
    }

    /// Codex r4138502157: a new link drops the mute timer of the old link.
    /// A mute of the new link that ends later gets its own timer.
    #[tokio::test]
    async fn a_new_link_drops_the_old_mute_timer() {
        let (session, tx, _rx) = linked();
        let now = inbox::now_ms();
        let mute = |end: i64| LinkEvent::Mute {
            jid: CHAT.into(),
            mute: Mute::UntilMs(end),
        };
        session.apply(mute(now + 60_000), 1, &tx);
        assert_eq!(armed_end(&session), Some(now + 60_000));

        session.begin(2);
        assert_eq!(armed_end(&session), None, "the old timer is gone");
        session.apply(LinkEvent::Connected, 2, &tx);
        session.apply(mute(now + 120_000), 2, &tx);
        assert_eq!(
            armed_end(&session),
            Some(now + 120_000),
            "a timer of link 2"
        );
    }

    /// #168 item 13: at the mute end the row stops being muted, with no new
    /// message. The window title counts its unread again.
    #[tokio::test]
    async fn the_mute_timer_unmutes_the_row_at_its_end() {
        let (session, tx, mut rx) = linked();
        session.apply(
            LinkEvent::Messages(vec![message(CHAT, "a", "one", 1)]),
            1,
            &tx,
        );
        let end = inbox::now_ms() + 100;
        let timed = LinkEvent::Mute {
            jid: CHAT.into(),
            mute: Mute::UntilMs(end),
        };
        session.apply(timed, 1, &tx);
        assert!(last_row(&mut rx).expect("row").muted);

        let row = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let AdapterEvent::ConversationUpsert { conversation } =
                    rx.recv().await.expect("the session is alive")
                {
                    return conversation;
                }
            }
        })
        .await
        .expect("the timer sends the row");
        assert!(!row.muted);
        assert_eq!(row.unread, 1, "its unread counts in the title again");
        assert!(inbox::now_ms() >= end);
    }

    /// #168 item 13: one timer waits for the earliest end. A reset stops it.
    #[tokio::test]
    async fn the_mute_timer_waits_for_the_earliest_end_and_stops_on_reset() {
        let (session, tx, _rx) = linked();
        let now = inbox::now_ms();
        let mute = |jid: &str, end: i64| LinkEvent::Mute {
            jid: jid.into(),
            mute: Mute::UntilMs(end),
        };
        session.apply(mute(CHAT, now + 120_000), 1, &tx);
        assert_eq!(armed_end(&session), Some(now + 120_000));
        session.apply(mute("222@s.whatsapp.net", now + 60_000), 1, &tx);
        assert_eq!(armed_end(&session), Some(now + 60_000), "the earlier end");
        session.apply(mute("333@s.whatsapp.net", now + 90_000), 1, &tx);
        assert_eq!(armed_end(&session), Some(now + 60_000), "a later end waits");

        session.reset();
        assert_eq!(armed_end(&session), None);
    }
}
