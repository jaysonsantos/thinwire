//! Scripted demo adapter (#120). Invented data only: no network, no
//! account, no secret.
//!
//! One [`DemoAdapter`] plays one protocol from a [`DemoScript`]. It answers
//! the commands of the shell the way a real adapter does (ADR 0010, adapter
//! contract), so the demo scenarios in `thinwire-core` drive the core through
//! `Intent` and `AdapterEvent` only. The UI snapshot tests and the headless
//! example use the same scenarios.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::adapter::{
    AccountState, AdapterCommand, AdapterError, AdapterEvent, AdapterStatus, ChatMessage,
    Conversation, Delivery, EventTx, ProtocolAdapter, ProtocolCapabilities, ProtocolId,
    TelegramAuthError, TelegramAuthPhase, TelegramAuthStep,
};

/// How the demo answers a `SendText`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DemoSend {
    /// The message goes out: a sent row and `SendAccepted`.
    #[default]
    Accept,
    /// The message fails: a failed row and `SendRejected`.
    Reject,
    /// No answer: a pending row stays, and the chat stays locked.
    Hold,
}

/// How the demo answers a `LoadOlderMessages`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DemoOlder {
    /// The page before the anchor, then `OlderHistoryLoaded`.
    #[default]
    Answer,
    /// No answer: the older page stays loading.
    Hold,
}

/// The answer to one Telegram login step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DemoLogin {
    /// Go to this phase.
    Phase(TelegramAuthPhase),
    /// Refuse the step with this error.
    Reject(TelegramAuthError),
}

/// Everything one demo protocol does.
#[derive(Debug, Clone)]
pub struct DemoScript {
    pub caps: ProtocolCapabilities,
    /// Events at start, in order. A linked demo starts with
    /// `Account { Linked }`, then its rows.
    pub start: Vec<AdapterEvent>,
    /// The whole history of each chat, oldest first.
    pub history: HashMap<String, Vec<ChatMessage>>,
    /// Messages in one history page: `OpenChat` sends the newest page.
    pub page: usize,
    pub send: DemoSend,
    pub older: DemoOlder,
    /// Telegram login: the answer to each step. A step with no entry gets
    /// no answer.
    pub login: Vec<(TelegramAuthStep, DemoLogin)>,
    /// The send time of messages that the demo sends: a fixed time, so a
    /// screen is the same on each run.
    pub now: i64,
}

impl DemoScript {
    /// A linked protocol with these rows and no history.
    #[must_use]
    pub fn linked(caps: ProtocolCapabilities, rows: Vec<Conversation>, now: i64) -> Self {
        let protocol = caps.id;
        let mut start = vec![
            AdapterEvent::Account {
                protocol,
                state: AccountState::Linked,
            },
            AdapterEvent::Status {
                protocol,
                status: AdapterStatus::Ready,
                detail: format!("{} is ready.", protocol.display_name()),
            },
        ];
        start.extend(
            rows.into_iter()
                .map(|conversation| AdapterEvent::ConversationUpsert { conversation }),
        );
        start.push(AdapterEvent::ChatListLoaded { protocol });
        Self {
            caps,
            start,
            history: HashMap::new(),
            page: 30,
            send: DemoSend::Accept,
            older: DemoOlder::Answer,
            login: Vec::new(),
            now,
        }
    }

    /// A protocol with no account and no start event. It still answers
    /// `Shutdown`, so a frontend that waits for every adapter to stop does
    /// not wait for its timeout (PR #122 review).
    #[must_use]
    pub fn silent(caps: ProtocolCapabilities, now: i64) -> Self {
        let mut script = Self::unlinked(caps, now);
        script.start.clear();
        script
    }

    /// A protocol with no account: it only reports that it is not set up.
    #[must_use]
    pub fn unlinked(caps: ProtocolCapabilities, now: i64) -> Self {
        let protocol = caps.id;
        Self {
            caps,
            start: vec![AdapterEvent::Status {
                protocol,
                status: AdapterStatus::Stubbed,
                detail: format!("{} is not set up.", protocol.display_name()),
            }],
            history: HashMap::new(),
            page: 30,
            send: DemoSend::Accept,
            older: DemoOlder::Answer,
            login: Vec::new(),
            now,
        }
    }
}

/// Work counts of the demo adapters of one host. A frontend compares them
/// with the commands it sent and the events it read, so it knows when every
/// answer is applied, with no timing guess (#120).
#[derive(Debug, Default)]
pub struct DemoCounters {
    started: AtomicU64,
    handled: AtomicU64,
    emitted: AtomicU64,
}

impl DemoCounters {
    /// Commands that the demo adapters handled, `ViewChat` and `Shutdown`
    /// too.
    #[must_use]
    pub fn handled(&self) -> u64 {
        self.handled.load(Ordering::SeqCst)
    }

    /// Demo adapters that ran `start` and sent their start events.
    #[must_use]
    pub fn started(&self) -> u64 {
        self.started.load(Ordering::SeqCst)
    }

    /// Events that the demo adapters sent.
    #[must_use]
    pub fn emitted(&self) -> u64 {
        self.emitted.load(Ordering::SeqCst)
    }
}

/// Plays one [`DemoScript`].
#[derive(Debug)]
pub struct DemoAdapter {
    script: DemoScript,
    counters: Arc<DemoCounters>,
}

impl DemoAdapter {
    #[must_use]
    pub fn new(script: DemoScript) -> Self {
        Self::with_counters(script, Arc::default())
    }

    /// A demo adapter that adds its work to shared counters.
    #[must_use]
    pub const fn with_counters(script: DemoScript, counters: Arc<DemoCounters>) -> Self {
        Self { script, counters }
    }

    fn send(&self, events: &EventTx, event: AdapterEvent) {
        self.counters.emitted.fetch_add(1, Ordering::SeqCst);
        let _ = events.send(event);
    }

    fn protocol(&self) -> ProtocolId {
        self.script.caps.id
    }

    fn open_chat(&self, chat: String, events: &EventTx) {
        let protocol = self.protocol();
        let Some(history) = self.script.history.get(&chat) else {
            self.send(
                events,
                AdapterEvent::CommandFailed {
                    protocol,
                    conversation_id: Some(chat),
                    detail: "The demo has no such chat.".into(),
                },
            );
            return;
        };
        let from = history.len().saturating_sub(self.script.page);
        for message in &history[from..] {
            self.send(
                events,
                AdapterEvent::MessageReceived {
                    message: message.clone(),
                },
            );
        }
        self.send(
            events,
            AdapterEvent::HistoryLoaded {
                protocol,
                conversation_id: chat,
            },
        );
    }

    fn load_older(&self, chat: String, before: String, events: &EventTx) {
        if self.script.older == DemoOlder::Hold {
            return;
        }
        let protocol = self.protocol();
        let history = self
            .script
            .history
            .get(&chat)
            .map_or(&[][..], Vec::as_slice);
        let end = history
            .iter()
            .position(|message| message.id == before)
            .unwrap_or(0);
        let from = end.saturating_sub(self.script.page);
        for message in &history[from..end] {
            self.send(
                events,
                AdapterEvent::MessageReceived {
                    message: message.clone(),
                },
            );
        }
        self.send(
            events,
            AdapterEvent::OlderHistoryLoaded {
                protocol,
                conversation_id: chat,
                before_message_id: before,
                more: from > 0,
                note: None,
            },
        );
    }

    fn send_text(&self, chat: String, body: String, request: u64, events: &EventTx) {
        let protocol = self.protocol();
        let delivery = match self.script.send {
            DemoSend::Accept => Delivery::Sent,
            DemoSend::Reject => Delivery::Failed,
            DemoSend::Hold => Delivery::Pending,
        };
        self.send(
            events,
            AdapterEvent::MessageReceived {
                message: ChatMessage {
                    protocol,
                    conversation_id: chat.clone(),
                    id: format!("{chat}:demo-send-{request}"),
                    sender: "You".into(),
                    body,
                    outbound: true,
                    delivery,
                    sent_at: self.script.now,
                },
            },
        );
        let answer = match self.script.send {
            DemoSend::Accept => AdapterEvent::SendAccepted {
                protocol,
                conversation_id: chat,
                request,
            },
            DemoSend::Reject => AdapterEvent::SendRejected {
                protocol,
                conversation_id: chat,
                request,
            },
            DemoSend::Hold => return,
        };
        self.send(events, answer);
    }

    fn resend(&self, chat: String, message_id: String, request: u64, events: &EventTx) {
        let protocol = self.protocol();
        let (delivery, answer) = match self.script.send {
            DemoSend::Hold => return,
            DemoSend::Accept => (
                Delivery::Sent,
                AdapterEvent::SendAccepted {
                    protocol,
                    conversation_id: chat.clone(),
                    request,
                },
            ),
            DemoSend::Reject => (
                Delivery::Failed,
                AdapterEvent::SendRejected {
                    protocol,
                    conversation_id: chat.clone(),
                    request,
                },
            ),
        };
        self.send(
            events,
            AdapterEvent::MessageDelivery {
                protocol,
                conversation_id: chat,
                message_id,
                delivery,
            },
        );
        self.send(events, answer);
    }

    fn login(&self, step: TelegramAuthStep, epoch: u64, events: &EventTx) {
        let Some((_, reply)) = self.script.login.iter().find(|(seen, _)| *seen == step) else {
            return;
        };
        let event = match *reply {
            DemoLogin::Phase(phase) => AdapterEvent::TelegramAuth { phase },
            DemoLogin::Reject(error) => AdapterEvent::TelegramAuthRejected { error },
        };
        self.send(
            events,
            AdapterEvent::Login {
                epoch,
                event: Box::new(event),
            },
        );
    }
}

impl ProtocolAdapter for DemoAdapter {
    fn id(&self) -> ProtocolId {
        self.protocol()
    }

    fn capabilities(&self) -> ProtocolCapabilities {
        self.script.caps
    }

    fn view_chat(&mut self, _conversation_id: Option<&str>, _events: &EventTx) {
        self.counters.handled.fetch_add(1, Ordering::SeqCst);
    }

    fn shutdown(&mut self, events: &EventTx) {
        let protocol = self.protocol();
        self.send(events, AdapterEvent::Stopped { protocol });
        self.counters.handled.fetch_add(1, Ordering::SeqCst);
    }

    fn start(&mut self, events: EventTx) {
        for event in &self.script.start {
            self.send(&events, event.clone());
        }
        self.counters.started.fetch_add(1, Ordering::SeqCst);
    }

    fn handle(&mut self, command: AdapterCommand, events: &EventTx) -> Result<(), AdapterError> {
        self.answer(command, events);
        // Count the command only after its events: then "all handled" also
        // means "all sent".
        self.counters.handled.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

impl DemoAdapter {
    fn answer(&self, command: AdapterCommand, events: &EventTx) {
        match command {
            AdapterCommand::OpenChat {
                conversation_id, ..
            } => self.open_chat(conversation_id, events),
            AdapterCommand::LoadOlderMessages {
                conversation_id,
                before_message_id,
                ..
            } => self.load_older(conversation_id, before_message_id, events),
            AdapterCommand::LoadChats { protocol } => {
                self.send(events, AdapterEvent::ChatListLoaded { protocol });
            }
            AdapterCommand::SendText {
                conversation_id,
                body,
                request,
                ..
            } => self.send_text(conversation_id, body, request, events),
            AdapterCommand::ResendMessage {
                conversation_id,
                message_id,
                request,
                ..
            } => self.resend(conversation_id, message_id, request, events),
            AdapterCommand::TelegramAuth { step, epoch } => self.login(step, epoch, events),
            AdapterCommand::Disconnect { protocol } => self.send(
                events,
                AdapterEvent::Account {
                    protocol,
                    state: AccountState::Unlinked,
                },
            ),
            // Connect and the pairing commands have no demo script.
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_772_445_600;

    fn row(id: &str) -> Conversation {
        Conversation {
            protocol: ProtocolId::Telegram,
            id: id.into(),
            title: "Demo".into(),
            participant: "Demo".into(),
            preview: String::new(),
            unread: 0,
            order: 1,
            last_at: NOW,
            is_group: false,
            writable: true,
            placeholder: false,
        }
    }

    fn message(chat: &str, index: usize) -> ChatMessage {
        ChatMessage {
            protocol: ProtocolId::Telegram,
            conversation_id: chat.into(),
            id: format!("{chat}:{index}"),
            sender: "Demo".into(),
            body: format!("message {index}"),
            outbound: false,
            delivery: Delivery::Sent,
            sent_at: NOW + i64::try_from(index).unwrap_or_default(),
        }
    }

    /// The demo adapter follows the adapter contract of the shell.
    #[tokio::test]
    async fn the_demo_adapter_follows_the_adapter_contract() {
        let mut script = DemoScript::linked(crate::fake::CAPABILITIES, vec![row("demo:1")], NOW);
        script.history.insert(
            "demo:1".into(),
            (0..5).map(|index| message("demo:1", index)).collect(),
        );
        let mut kit = crate::contract::Contract::new(Box::new(DemoAdapter::new(script)));
        kit.linked().await;
        kit.run_all().await;
    }
}
