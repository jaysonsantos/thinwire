//! Shared WhatsApp session state between the adapter and the live client.
//!
//! The live client (feature `whatsapp-web`) turns whatsapp-rust events into
//! [`LinkEvent`] values. Tests feed the same values from fakes. Pairing
//! material travels only inside [`RedactedPairingSecret`].

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};

use super::inbox::{HistoryChat, Inbox, WaMessage};
use crate::adapter::{
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
        state.connected = false;
        state.sender = None;
        state.stopped = None;
        state.generation = NO_LINK;
        state.inbox.clear()
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
                status(events, AdapterStatus::Ready, CONNECTED);
                // ADR 0010 rule 1: Linked comes before the first inbox event.
                let mut out = vec![account(AccountState::Linked)];
                out.push(AdapterEvent::ConversationRemoved {
                    protocol: ProtocolId::WhatsApp,
                    id: super::PLACEHOLDER_ID.into(),
                });
                out.extend(state.inbox.chat_page());
                out
            }
            LinkEvent::Disconnected => {
                state.connected = false;
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
            LinkEvent::History { chats, push_names } => {
                state.inbox.apply_history(chats, push_names)
            }
            LinkEvent::Messages(messages) => state.inbox.apply_messages(messages),
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
    }

    /// Apply the result of a send that started on link `generation`.
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
        events: &EventTx,
    ) -> bool {
        let mut state = self.lock();
        if state.generation != generation {
            return false;
        }
        let revoked = result == Err(SendFailure::Unlinked);
        let out = match result {
            Ok(server_id) => state.inbox.confirm_send(jid, pending, server_id),
            Err(failure) => {
                status(events, AdapterStatus::Error, failure.detail());
                let mut out = state.inbox.fail_send(jid, pending);
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
