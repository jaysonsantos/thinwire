//! Adapter contract kit (ADR 0010, "Adapter contract for the shell").
//!
//! One test kit for every adapter. A test links its adapter against its own
//! offline fake and gives it to [`Contract`]. The kit sends commands through
//! the host's `dispatch`, with the same routing and error handling as the
//! app. It checks the events against the contract rules. A failed check
//! names its rule.
//!
//! Usage in an adapter's tests:
//!
//! ```ignore
//! let mut kit = Contract::new(Box::new(adapter));
//! kit.send(/* the protocol's own connect command */);
//! kit.linked().await;
//! kit.run_all().await;
//! ```

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use crate::adapter::{
    AccountState, AdapterCommand, AdapterEvent, AdapterStatus, EventTx, ProtocolAdapter,
    ProtocolCapabilities, ProtocolId,
};
use crate::host::dispatch;

/// Longest wait for one answer. The fakes answer in milliseconds.
const ANSWER_WAIT: Duration = Duration::from_secs(2);
/// A time with no event that ends [`Contract::settle`].
const QUIET: Duration = Duration::from_millis(50);
/// The first request id of the kit. It is far above the core's ids, so an
/// answer to a request that the kit did not send is easy to see.
const FIRST_REQUEST: u64 = 900_000;
/// A chat id that no adapter knows.
const UNKNOWN_CHAT: &str = "contract:unknown-chat";
/// A message id that no adapter knows.
const UNKNOWN_MESSAGE: &str = "contract:unknown-message";
/// The text of the kit's test send.
const SEND_BODY: &str = "contract kit send";

/// One adapter under test, and every event it sent.
pub(crate) struct Contract {
    /// One adapter, in the form the host's `dispatch` takes.
    adapters: Vec<Box<dyn ProtocolAdapter>>,
    protocol: ProtocolId,
    caps: ProtocolCapabilities,
    tx: EventTx,
    rx: UnboundedReceiver<AdapterEvent>,
    seen: Vec<AdapterEvent>,
    /// Send and retry requests that the kit sent.
    requests: HashSet<u64>,
    next_request: u64,
    /// Pairing generations that the kit sent (rule 8).
    generations: HashSet<u64>,
}

impl Contract {
    /// Start the adapter on a new event channel.
    pub(crate) fn new(mut adapter: Box<dyn ProtocolAdapter>) -> Self {
        let (tx, rx) = unbounded_channel();
        let protocol = adapter.id();
        let caps = adapter.capabilities();
        adapter.start(tx.clone());
        Self {
            adapters: vec![adapter],
            protocol,
            caps,
            tx,
            rx,
            seen: Vec::new(),
            requests: HashSet::new(),
            next_request: FIRST_REQUEST,
            generations: HashSet::new(),
        }
    }

    /// Check against other capabilities. For an adapter whose test build
    /// reports other capabilities than its feature build.
    pub(crate) fn with_capabilities(mut self, caps: ProtocolCapabilities) -> Self {
        self.caps = caps;
        self
    }

    /// The adapter's event channel. A test that drives a fake client (for
    /// example WhatsApp link events) sends its events here.
    pub(crate) fn events(&self) -> EventTx {
        self.tx.clone()
    }

    /// Send one command the way the host does.
    pub(crate) fn send(&mut self, command: AdapterCommand) {
        if let AdapterCommand::WhatsAppBeginLink { generation } = &command {
            self.generations.insert(*generation);
        }
        dispatch(&mut self.adapters, command, &self.tx);
    }

    /// Wait until an event matches. Fails after [`ANSWER_WAIT`] and names
    /// the rule.
    pub(crate) async fn until(
        &mut self,
        rule: &str,
        what: &str,
        test: impl Fn(&AdapterEvent) -> bool,
    ) -> AdapterEvent {
        loop {
            let event = tokio::time::timeout(ANSWER_WAIT, self.rx.recv())
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "contract rule {rule}: no {what} in {ANSWER_WAIT:?}. Last events: {:?}",
                        self.tail()
                    )
                })
                .expect("event channel open");
            self.seen.push(event.clone());
            if test(&event) {
                return event;
            }
        }
    }

    /// Read events until none comes for [`QUIET`].
    pub(crate) async fn settle(&mut self) {
        while let Ok(Some(event)) = tokio::time::timeout(QUIET, self.rx.recv()).await {
            self.seen.push(event);
        }
    }

    /// Wait for `Account { Linked }` (rule 1).
    pub(crate) async fn linked(&mut self) {
        let protocol = self.protocol;
        self.until("1", "Account Linked", |event| {
            is_account(event, protocol, AccountState::Linked)
        })
        .await;
        self.settle().await;
    }

    /// Run every check on a linked adapter, in a fixed order. It ends with
    /// a disconnect and a shutdown.
    pub(crate) async fn run_all(mut self) {
        let chat = if self.caps.sends_text {
            self.writable_chat()
                .expect("rule 3: a protocol that sends text lists a writable chat")
        } else {
            self.any_chat().expect("the adapter lists a chat")
        };
        self.check_load_chats().await;
        self.check_open_chat(&chat).await;
        self.check_open_unknown_chat().await;
        if self.caps.sends_text {
            self.check_send(&chat).await;
            self.check_resend_unknown(&chat).await;
        }
        self.check_view_chat(&chat).await;
        self.check_disconnect().await;
        self.check_shutdown().await;
        self.check_stream();
    }

    /// Rule 9: `LoadChats` ends with `ChatListLoaded` or a failure. Else
    /// the chat-list spinner never stops.
    pub(crate) async fn check_load_chats(&mut self) {
        self.send(AdapterCommand::LoadChats {
            protocol: self.protocol,
        });
        let protocol = self.protocol;
        self.until("9", "answer to LoadChats", |event| {
            matches!(event, AdapterEvent::ChatListLoaded { protocol: seen } if *seen == protocol)
                || ends_load(event, protocol, None)
        })
        .await;
        self.settle().await;
    }

    /// Rule 9: `OpenChat` ends with
    /// `HistoryLoaded` or a failure for the chat. Else the thread spinner
    /// never stops.
    pub(crate) async fn check_open_chat(&mut self, chat: &str) {
        self.send(AdapterCommand::OpenChat {
            protocol: self.protocol,
            conversation_id: chat.to_owned(),
        });
        let protocol = self.protocol;
        self.until("9", "answer to OpenChat", |event| {
            matches!(
                event,
                AdapterEvent::HistoryLoaded { protocol: seen, conversation_id }
                    if *seen == protocol && conversation_id == chat
            ) || ends_load(event, protocol, Some(chat))
        })
        .await;
        self.settle().await;
    }

    /// Rule 5: an unknown chat fails the command, not the session.
    pub(crate) async fn check_open_unknown_chat(&mut self) {
        let from = self.seen.len();
        self.check_open_chat(UNKNOWN_CHAT).await;
        let protocol = self.protocol;
        assert!(
            !self.seen[from..].iter().any(|event| is_account(
                event,
                protocol,
                AccountState::Unlinked
            )),
            "contract rule 5: a failed OpenChat ended the session"
        );
    }

    /// Rule 4: `SendText` gets `SendAccepted` or `SendRejected` for its
    /// request.
    pub(crate) async fn check_send(&mut self, chat: &str) {
        let request = self.request();
        self.send(AdapterCommand::SendText {
            protocol: self.protocol,
            conversation_id: chat.to_owned(),
            body: SEND_BODY.into(),
            request,
        });
        self.until("4", "answer to SendText", |event| {
            answer_of(event) == Some(request)
        })
        .await;
        self.settle().await;
    }

    /// Rule 4: a retry of a message that the adapter does not know still
    /// gets an answer for its request.
    pub(crate) async fn check_resend_unknown(&mut self, chat: &str) {
        let request = self.request();
        self.send(AdapterCommand::ResendMessage {
            protocol: self.protocol,
            conversation_id: chat.to_owned(),
            message_id: UNKNOWN_MESSAGE.into(),
            request,
        });
        self.until("4", "answer to ResendMessage", |event| {
            answer_of(event) == Some(request)
        })
        .await;
        self.settle().await;
    }

    /// Rule 7: `ViewChat` is a hint. It never ends the session.
    pub(crate) async fn check_view_chat(&mut self, chat: &str) {
        let from = self.seen.len();
        for conversation_id in [Some(chat.to_owned()), None] {
            self.send(AdapterCommand::ViewChat {
                protocol: self.protocol,
                conversation_id,
            });
        }
        self.settle().await;
        let protocol = self.protocol;
        assert!(
            !self.seen[from..].iter().any(|event| {
                is_account(event, protocol, AccountState::Unlinked)
                    || matches!(event, AdapterEvent::Stopped { .. })
            }),
            "contract rule 7: ViewChat ended the session"
        );
    }

    /// Rule 1: `Disconnect` ends the session with `Account { Unlinked }`.
    pub(crate) async fn check_disconnect(&mut self) {
        self.send(AdapterCommand::Disconnect {
            protocol: self.protocol,
        });
        let protocol = self.protocol;
        self.until("1", "Account Unlinked after Disconnect", |event| {
            is_account(event, protocol, AccountState::Unlinked)
        })
        .await;
        self.settle().await;
    }

    /// `Shutdown` ends with exactly one `Stopped` (`ProtocolAdapter::shutdown`).
    pub(crate) async fn check_shutdown(&mut self) {
        self.send(AdapterCommand::Shutdown {
            protocol: self.protocol,
        });
        let protocol = self.protocol;
        self.until(
            "shutdown",
            "Stopped",
            |event| matches!(event, AdapterEvent::Stopped { protocol: seen } if *seen == protocol),
        )
        .await;
        self.settle().await;
        let stopped = self
            .seen
            .iter()
            .filter(|event| matches!(event, AdapterEvent::Stopped { .. }))
            .count();
        assert_eq!(stopped, 1, "contract: Shutdown sends Stopped once");
    }

    /// The rules that hold for the whole event stream. Fails with every
    /// violation.
    pub(crate) fn check_stream(&self) {
        let found = violations(
            self.protocol,
            &self.caps,
            &self.seen,
            &self.requests,
            &self.generations,
        );
        assert!(found.is_empty(), "contract violations: {found:#?}");
    }

    fn request(&mut self) -> u64 {
        let request = self.next_request;
        self.next_request += 1;
        self.requests.insert(request);
        request
    }

    /// The latest row of each chat, in the order the chats first came.
    fn rows(&self) -> Vec<crate::Conversation> {
        let mut order = Vec::new();
        let mut rows = HashMap::new();
        for event in &self.seen {
            if let AdapterEvent::ConversationUpsert { conversation } = event {
                if !rows.contains_key(&conversation.id) {
                    order.push(conversation.id.clone());
                }
                rows.insert(conversation.id.clone(), conversation.clone());
            }
        }
        order
            .into_iter()
            .filter_map(|id| rows.remove(&id))
            .collect()
    }

    fn writable_chat(&self) -> Option<String> {
        self.rows()
            .into_iter()
            .find(|row| row.writable && !row.placeholder)
            .map(|row| row.id)
    }

    fn any_chat(&self) -> Option<String> {
        self.rows()
            .into_iter()
            .find(|row| !row.placeholder)
            .map(|row| row.id)
    }

    fn tail(&self) -> &[AdapterEvent] {
        &self.seen[self.seen.len().saturating_sub(8)..]
    }
}

fn is_account(event: &AdapterEvent, protocol: ProtocolId, state: AccountState) -> bool {
    matches!(
        event,
        AdapterEvent::Account { protocol: seen, state: now } if *seen == protocol && *now == state
    )
}

/// The request of a send answer.
fn answer_of(event: &AdapterEvent) -> Option<u64> {
    match event {
        AdapterEvent::SendAccepted { request, .. } | AdapterEvent::SendRejected { request, .. } => {
            Some(*request)
        }
        _ => None,
    }
}

/// A failure that ends a load: a failed command for the chat or for no
/// chat, or an error status (the host sends one when `handle` fails, and
/// the core stops the spinners on it).
fn ends_load(event: &AdapterEvent, protocol: ProtocolId, chat: Option<&str>) -> bool {
    match event {
        AdapterEvent::CommandFailed {
            protocol: seen,
            conversation_id,
            ..
        } => *seen == protocol && (conversation_id.is_none() || conversation_id.as_deref() == chat),
        AdapterEvent::Status {
            protocol: seen,
            status: AdapterStatus::Error | AdapterStatus::Refused,
            ..
        } => *seen == protocol,
        _ => false,
    }
}

/// The protocol that an event names, if it names one.
fn event_protocol(event: &AdapterEvent) -> Option<ProtocolId> {
    match event {
        AdapterEvent::Status { protocol, .. }
        | AdapterEvent::Stopped { protocol }
        | AdapterEvent::Account { protocol, .. }
        | AdapterEvent::CommandFailed { protocol, .. }
        | AdapterEvent::Notice { protocol, .. } => Some(*protocol),
        other => other.inbox_protocol(),
    }
}

/// Every rule violation in an event stream of one adapter.
///
/// - Rule 1: no inbox event before `Account { Linked }` or after
///   `Account { Unlinked }`. A `Linking` after `Linked` keeps the session.
/// - Every event names the adapter's own protocol.
/// - Rule 3: a placeholder row is not writable. A protocol that does not
///   send text lists no writable row.
/// - Rule 4: each send answer is for a request that was sent, and each
///   request gets one answer at most.
/// - Rule 8: each pairing payload carries a generation that was sent.
pub(crate) fn violations(
    protocol: ProtocolId,
    caps: &ProtocolCapabilities,
    events: &[AdapterEvent],
    requests: &HashSet<u64>,
    generations: &HashSet<u64>,
) -> Vec<String> {
    let mut found = Vec::new();
    let mut session = false;
    let mut answers: HashMap<u64, usize> = HashMap::new();
    for (index, event) in events.iter().enumerate() {
        if let Some(named) = event_protocol(event)
            && named != protocol
        {
            found.push(format!(
                "#{index}: names {named}, not {protocol}: {event:?}"
            ));
        }
        if let AdapterEvent::Account { state, .. } = event {
            match state {
                AccountState::Linked => session = true,
                AccountState::Unlinked => session = false,
                AccountState::Linking => {}
            }
        }
        if event.inbox_protocol().is_some() && !session {
            found.push(format!(
                "#{index}: rule 1: inbox event with no linked session: {event:?}"
            ));
        }
        if let AdapterEvent::ConversationUpsert { conversation } = event {
            if conversation.placeholder && conversation.writable {
                found.push(format!(
                    "#{index}: rule 3: placeholder row {} is writable",
                    conversation.id
                ));
            }
            if !caps.sends_text && conversation.writable {
                found.push(format!(
                    "#{index}: rule 3: row {} is writable, but sends_text is false",
                    conversation.id
                ));
            }
        }
        if let Some(request) = answer_of(event) {
            if !requests.contains(&request) {
                found.push(format!(
                    "#{index}: rule 4: answer for request {request}, which was not sent"
                ));
            }
            let count = answers.entry(request).or_default();
            *count += 1;
            if *count == 2 {
                found.push(format!(
                    "#{index}: rule 4: second answer for request {request}"
                ));
            }
        }
        if let AdapterEvent::WhatsAppQr { generation, .. }
        | AdapterEvent::WhatsAppPairCode { generation, .. } = event
            && !generations.contains(generation)
        {
            found.push(format!(
                "#{index}: rule 8: pairing payload of generation {generation}, which was not sent"
            ));
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{ChatMessage, Conversation, Delivery};

    const CAPS: ProtocolCapabilities = crate::fake::CAPABILITIES;

    fn linked(state: AccountState) -> AdapterEvent {
        AdapterEvent::Account {
            protocol: ProtocolId::Telegram,
            state,
        }
    }

    fn row(id: &str, writable: bool, placeholder: bool) -> AdapterEvent {
        AdapterEvent::ConversationUpsert {
            conversation: Conversation {
                protocol: ProtocolId::Telegram,
                id: id.into(),
                title: id.into(),
                participant: String::new(),
                preview: String::new(),
                unread: 0,
                order: 0,
                last_at: 0,
                is_group: false,
                writable,
                placeholder,
            },
        }
    }

    fn accepted(request: u64) -> AdapterEvent {
        AdapterEvent::SendAccepted {
            protocol: ProtocolId::Telegram,
            conversation_id: "c1".into(),
            request,
        }
    }

    fn check(events: &[AdapterEvent]) -> Vec<String> {
        violations(
            ProtocolId::Telegram,
            &CAPS,
            events,
            &HashSet::from([1]),
            &HashSet::new(),
        )
    }

    /// A stream that follows the rules has no violation. A reconnect
    /// (`Linking` after `Linked`) keeps the session.
    #[test]
    fn a_good_stream_has_no_violation() {
        let events = [
            linked(AccountState::Linked),
            row("c1", true, false),
            linked(AccountState::Linking),
            accepted(1),
            linked(AccountState::Linked),
            linked(AccountState::Unlinked),
        ];
        assert_eq!(check(&events), Vec::<String>::new());
    }

    /// Each rule that the stream check covers finds its violation.
    #[test]
    fn each_violation_is_found() {
        let message = AdapterEvent::MessageReceived {
            message: ChatMessage {
                protocol: ProtocolId::Telegram,
                conversation_id: "c1".into(),
                id: "m1".into(),
                sender: String::new(),
                body: String::new(),
                outbound: false,
                delivery: Delivery::Sent,
                sent_at: 0,
            },
        };
        let events = [
            message.clone(),
            linked(AccountState::Linked),
            row("c1", true, true),
            accepted(1),
            accepted(1),
            accepted(2),
            AdapterEvent::Notice {
                protocol: ProtocolId::Slack,
                text: String::new(),
            },
            linked(AccountState::Unlinked),
            message,
        ];
        let found = check(&events);
        let has = |part: &str| found.iter().any(|line| line.contains(part));
        assert!(has("#0: rule 1"), "{found:#?}");
        assert!(has("#2: rule 3: placeholder"), "{found:#?}");
        assert!(has("#4: rule 4: second answer"), "{found:#?}");
        assert!(has("#5: rule 4: answer for request 2"), "{found:#?}");
        assert!(has("#6: names Slack"), "{found:#?}");
        assert!(has("#8: rule 1"), "{found:#?}");
        assert_eq!(found.len(), 6, "{found:#?}");
    }

    /// A protocol that does not send text lists no writable row.
    #[test]
    fn a_writable_row_needs_sends_text() {
        let caps = ProtocolCapabilities {
            sends_text: false,
            ..CAPS
        };
        let found = violations(
            ProtocolId::Telegram,
            &caps,
            &[linked(AccountState::Linked), row("c1", true, false)],
            &HashSet::new(),
            &HashSet::new(),
        );
        assert_eq!(found.len(), 1, "{found:#?}");
        assert!(found[0].contains("sends_text is false"));
    }

    /// Rule 8: a pairing payload of a generation that was not sent.
    #[test]
    fn a_pairing_payload_of_an_unknown_generation_is_found() {
        let qr = AdapterEvent::WhatsAppQr {
            code: crate::RedactedPairingSecret::new("qr"),
            generation: 7,
        };
        let found = violations(
            ProtocolId::WhatsApp,
            &CAPS,
            std::slice::from_ref(&qr),
            &HashSet::new(),
            &HashSet::from([6]),
        );
        assert_eq!(found.len(), 1, "{found:#?}");
        assert!(found[0].contains("rule 8"));
        let fine = violations(
            ProtocolId::WhatsApp,
            &CAPS,
            &[qr],
            &HashSet::new(),
            &HashSet::from([7]),
        );
        assert!(fine.is_empty(), "{fine:#?}");
    }

    /// The fake adapter follows the whole contract.
    #[tokio::test]
    async fn the_fake_adapter_follows_the_contract() {
        let mut kit = Contract::new(Box::new(crate::FakeAdapter::default()));
        kit.linked().await;
        kit.send(AdapterCommand::Connect {
            protocol: ProtocolId::Telegram,
        });
        kit.settle().await;
        kit.run_all().await;
    }
}
