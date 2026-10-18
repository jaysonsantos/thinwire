//! Tests of the helper adapter with a fake Signal helper, and with the
//! Signal and the WhatsApp helper side by side (#248).
//!
//! Each fake helper is a tokio task behind in-memory pipes. It runs the
//! real [`serve`] loop over a small fake client. So the tests cover the wire
//! protocol from both sides with no AGPL code and no child process.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tokio::sync::oneshot;

use super::launch::{HelperLauncher, HelperProcess};
use super::serve::{ServeConfig, serve};
use super::supervisor::{HelperAdapter, HelperSpec, HelperTiming};
use crate::adapter::{
    AccountState, AdapterCommand, AdapterError, AdapterEvent, AdapterStatus, Arrival, ChatMessage,
    Conversation, Delivery, EventTx, HelperFault, HelperState, ProtocolAdapter,
    ProtocolCapabilities, ProtocolId, RedactedPairingSecret, SupportClass, emit_account,
    emit_chat_list_loaded, emit_conversation, emit_message, emit_send_accepted, emit_status,
    emit_stopped,
};

const CHAT: &str = "chat-1";
/// The provisioning URL of the fake Signal client.
const SIGNAL_URL: &str = "sgnl://linkdevice?uuid=fake";
/// Size of the in-memory pipes.
const PIPE_BYTES: usize = 64 * 1024;
/// Longest wait for one event in a test.
const EVENT_WAIT: Duration = Duration::from_secs(5);
/// A time in which every restart timer of [`TIMING`] fires, also on a slow
/// test computer.
const TIMERS_FIRED: Duration = Duration::from_millis(400);

/// Short waits, so a restart test takes milliseconds. The third failure in
/// a row stops the helper for good.
const TIMING: HelperTiming = HelperTiming {
    hello_wait: Duration::from_secs(2),
    busy_after: Duration::from_millis(50),
    restart_base: Duration::from_millis(20),
    restart_max: Duration::from_millis(100),
    max_failures: 3,
    stable_run: Duration::from_secs(60),
    stop_wait: Duration::from_millis(300),
    kill_wait: Duration::from_secs(2),
    drain_wait: Duration::from_secs(2),
};

const fn caps(protocol: ProtocolId) -> ProtocolCapabilities {
    ProtocolCapabilities {
        id: protocol,
        support: SupportClass::Experimental,
        short_label: "fake helper",
        detail: "A fake helper for tests.",
        official_api: false,
        allows_user_account_automation: false,
        sends_text: true,
        pages_history: true,
    }
}

// region: fake helper

/// One fake helper program: what the test set up, and what it did.
struct Site {
    protocol: ProtocolId,
    launches: AtomicU32,
    /// Ends the helper that runs, like a crash.
    crash: Notify,
}

impl Site {
    fn new(protocol: ProtocolId) -> Arc<Self> {
        Arc::new(Self {
            protocol,
            launches: AtomicU32::new(0),
            crash: Notify::new(),
        })
    }

    fn launches(&self) -> u32 {
        self.launches.load(Ordering::SeqCst)
    }
}

struct SiteLauncher(Arc<Site>);

impl HelperLauncher for SiteLauncher {
    fn installed(&self) -> bool {
        true
    }

    fn launch(&self) -> std::io::Result<HelperProcess> {
        let site = Arc::clone(&self.0);
        site.launches.fetch_add(1, Ordering::SeqCst);
        let (stdin, helper_in) = tokio::io::duplex(PIPE_BYTES);
        let (helper_out, stdout) = tokio::io::duplex(PIPE_BYTES);
        let (exited_tx, exited) = oneshot::channel();
        let (kill, killed) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let client = FakeClient {
                protocol: site.protocol,
                acknowledged: false,
                linked: false,
            };
            let config = ServeConfig {
                helper_version: "fake",
                // As the real Signal helper: no phone vault.
                phone: None,
            };
            tokio::select! {
                _ = serve(Box::new(client), config, helper_in, helper_out) => {}
                () = site.crash.notified() => {}
                _ = killed => {}
            }
            let _ = exited_tx.send(());
        });
        Ok(HelperProcess {
            stdin: Box::new(stdin),
            stdout: Box::new(stdout),
            stderr: None,
            exited,
            kill,
            pid: None,
        })
    }
}

/// A client of one protocol that links at once, with no network.
struct FakeClient {
    protocol: ProtocolId,
    acknowledged: bool,
    linked: bool,
}

impl FakeClient {
    fn chat(&self) -> Conversation {
        Conversation {
            protocol: self.protocol,
            id: CHAT.into(),
            title: "Ana".into(),
            participant: "Ana".into(),
            preview: "hello".into(),
            unread: 0,
            order: 1,
            last_at: 1_790_000_000,
            is_group: false,
            writable: true,
            placeholder: false,
            muted: false,
            has_topics: false,
            avatar: Default::default(),
        }
    }

    fn begin(&mut self, generation: u64, events: &EventTx) -> Result<(), AdapterError> {
        if !self.acknowledged {
            return Err(AdapterError::Refused {
                protocol: self.protocol,
                reason: "the gate was not accepted",
            });
        }
        let protocol = self.protocol;
        emit_account(events, protocol, AccountState::Linking);
        let code = RedactedPairingSecret::new(SIGNAL_URL);
        let _ = events.send(if protocol == ProtocolId::Signal {
            AdapterEvent::SignalQr { code, generation }
        } else {
            AdapterEvent::WhatsAppQr { code, generation }
        });
        self.linked = true;
        emit_account(events, protocol, AccountState::Linked);
        emit_status(events, protocol, AdapterStatus::Ready, "linked");
        Ok(())
    }
}

impl ProtocolAdapter for FakeClient {
    fn id(&self) -> ProtocolId {
        self.protocol
    }

    fn capabilities(&self) -> ProtocolCapabilities {
        caps(self.protocol)
    }

    fn start(&mut self, events: EventTx) {
        emit_status(
            &events,
            self.protocol,
            AdapterStatus::Stubbed,
            "fake helper up",
        );
    }

    fn shutdown(&mut self, events: &EventTx) {
        emit_stopped(events, self.protocol);
    }

    fn handle(&mut self, command: AdapterCommand, events: &EventTx) -> Result<(), AdapterError> {
        let protocol = self.protocol;
        match command {
            AdapterCommand::SignalAcknowledgeNotice | AdapterCommand::WhatsAppAcknowledgeRisk => {
                self.acknowledged = true;
            }
            AdapterCommand::SignalBeginLink { generation }
            | AdapterCommand::WhatsAppBeginLink { generation } => {
                return self.begin(generation, events);
            }
            AdapterCommand::LoadChats { .. } => {
                if self.linked {
                    emit_conversation(events, self.chat());
                }
                emit_chat_list_loaded(events, protocol);
            }
            AdapterCommand::SendText {
                conversation_id,
                body,
                request,
                ..
            } => {
                emit_message(
                    events,
                    ChatMessage {
                        protocol,
                        conversation_id: conversation_id.clone(),
                        id: format!("sent:{request}"),
                        sender: "me".into(),
                        body,
                        outbound: true,
                        delivery: Delivery::Sent,
                        sent_at: 1_790_000_000,
                        arrival: Arrival::History,
                        topic: None,
                        links: Vec::new(),
                        photo: None,
                    },
                );
                emit_send_accepted(events, protocol, conversation_id, request);
            }
            AdapterCommand::Disconnect { .. }
            | AdapterCommand::SignalCancelLink
            | AdapterCommand::WhatsAppCancelLink => {
                self.acknowledged = false;
                self.linked = false;
                emit_account(events, protocol, AccountState::Unlinked);
            }
            _ => {}
        }
        Ok(())
    }
}

// endregion: fake helper

// region: probe

/// The helper adapter of one protocol on its own event channel.
struct Probe {
    protocol: ProtocolId,
    adapter: HelperAdapter,
    site: Arc<Site>,
    tx: EventTx,
    rx: UnboundedReceiver<AdapterEvent>,
    seen: Vec<AdapterEvent>,
}

impl Probe {
    fn start(protocol: ProtocolId) -> Self {
        let site = Site::new(protocol);
        let mut adapter = HelperAdapter::new(
            HelperSpec {
                protocol,
                capabilities: caps(protocol),
            },
            Arc::new(SiteLauncher(Arc::clone(&site))),
            None,
        )
        .with_timing(TIMING);
        let (tx, rx) = unbounded_channel();
        adapter.start(tx.clone());
        Self {
            protocol,
            adapter,
            site,
            tx,
            rx,
            seen: Vec::new(),
        }
    }

    fn send(&mut self, command: AdapterCommand) {
        self.adapter
            .handle(command, &self.tx)
            .expect("the helper adapter takes the command");
    }

    /// Accept the gate and start link number `generation`.
    fn link(&mut self, generation: u64) {
        if self.protocol == ProtocolId::Signal {
            self.send(AdapterCommand::SignalAcknowledgeNotice);
            self.send(AdapterCommand::SignalBeginLink { generation });
        } else {
            self.send(AdapterCommand::WhatsAppAcknowledgeRisk);
            self.send(AdapterCommand::WhatsAppBeginLink { generation });
        }
    }

    async fn until(&mut self, what: &str, test: impl Fn(&AdapterEvent) -> bool) -> AdapterEvent {
        loop {
            let event = tokio::time::timeout(EVENT_WAIT, self.rx.recv())
                .await
                .unwrap_or_else(|_| panic!("no {what} in {EVENT_WAIT:?}. Seen: {:?}", self.seen))
                .expect("event channel open");
            self.seen.push(event.clone());
            if test(&event) {
                return event;
            }
        }
    }

    async fn until_helper(&mut self, state: HelperState) {
        let protocol = self.protocol;
        self.until(&format!("Helper {state:?}"), |event| {
            *event == AdapterEvent::Helper { protocol, state }
        })
        .await;
    }

    async fn until_account(&mut self, state: AccountState) {
        let protocol = self.protocol;
        self.until(&format!("Account {state:?}"), |event| {
            *event == AdapterEvent::Account { protocol, state }
        })
        .await;
    }

    /// Read every event that is in the channel now.
    fn drain(&mut self) {
        while let Ok(event) = self.rx.try_recv() {
            self.seen.push(event);
        }
    }

    fn saw(&self, test: impl Fn(&AdapterEvent) -> bool) -> bool {
        self.seen.iter().any(test)
    }

    async fn shut_down(mut self) {
        let protocol = self.protocol;
        self.adapter.shutdown(&self.tx);
        self.until("Stopped", |event| {
            *event == AdapterEvent::Stopped { protocol }
        })
        .await;
    }
}

// endregion: probe

/// #248 "IPC round-trip tests with a fake Signal helper": the notice starts
/// the helper, the link gives a provisioning URL with the generation of the
/// shell, and loads and sends cross the pipe both ways. Every event names
/// Signal, and no phone number is part of a Signal link.
#[tokio::test]
async fn a_signal_link_crosses_the_pipe_both_ways() {
    const PROTOCOL: ProtocolId = ProtocolId::Signal;
    let mut probe = Probe::start(PROTOCOL);
    assert_eq!(probe.site.launches(), 0, "no helper before the notice");
    probe.link(4);
    probe.until_helper(HelperState::Running).await;
    let qr = probe
        .until("SignalQr", |event| {
            matches!(event, AdapterEvent::SignalQr { .. })
        })
        .await;
    let AdapterEvent::SignalQr { code, generation } = qr else {
        unreachable!("matched above");
    };
    assert_eq!(generation, 4, "the link of the shell, not another one");
    assert_eq!(code.reveal(), SIGNAL_URL);
    probe.until_account(AccountState::Linked).await;

    probe.send(AdapterCommand::LoadChats { protocol: PROTOCOL });
    let row = probe
        .until("ConversationUpsert", |event| {
            matches!(event, AdapterEvent::ConversationUpsert { .. })
        })
        .await;
    let AdapterEvent::ConversationUpsert { conversation } = row else {
        unreachable!("matched above");
    };
    assert_eq!(conversation.protocol, PROTOCOL);
    assert_eq!(conversation.id, CHAT);
    probe
        .until("ChatListLoaded", |event| {
            *event == AdapterEvent::ChatListLoaded { protocol: PROTOCOL }
        })
        .await;

    probe.send(AdapterCommand::SendText {
        protocol: PROTOCOL,
        conversation_id: CHAT.into(),
        body: "hello".into(),
        request: 9,
    });
    probe
        .until("SendAccepted", |event| {
            *event
                == AdapterEvent::SendAccepted {
                    protocol: PROTOCOL,
                    conversation_id: CHAT.into(),
                    request: 9,
                }
        })
        .await;
    assert!(
        !probe.saw(|event| matches!(
            event,
            AdapterEvent::WhatsAppQr { .. } | AdapterEvent::WhatsAppPairCode { .. }
        )),
        "a Signal helper sends no WhatsApp payload"
    );
    assert_eq!(probe.site.launches(), 1);
    probe.shut_down().await;
}

/// #248 "Helper crash test": the Signal helper ends each time the app
/// starts it. After the last try the adapter reports `Stopped`, which the
/// account row shows as "Helper stopped." with Restart. The WhatsApp helper
/// is another process with its own adapter: it keeps running, it stays
/// linked, and it still answers. Restart then links Signal again.
#[tokio::test]
async fn a_stopped_signal_helper_leaves_the_whatsapp_helper_running() {
    let mut whatsapp = Probe::start(ProtocolId::WhatsApp);
    let mut signal = Probe::start(ProtocolId::Signal);
    whatsapp.link(1);
    whatsapp.until_account(AccountState::Linked).await;
    signal.link(2);
    signal.until_account(AccountState::Linked).await;
    whatsapp.drain();
    let whatsapp_events = whatsapp.seen.len();

    // Three failures in a row: two restarts, then the stop.
    for _ in 0..2 {
        signal.site.crash.notify_one();
        signal.until_helper(HelperState::Restarting).await;
        signal.until_account(AccountState::Linked).await;
    }
    signal.site.crash.notify_one();
    signal
        .until_helper(HelperState::Stopped(HelperFault::Crashed))
        .await;
    assert!(
        signal.saw(|event| *event
            == AdapterEvent::Account {
                protocol: ProtocolId::Signal,
                state: AccountState::Unlinked,
            }),
        "the Signal account is not linked while its helper is stopped"
    );
    tokio::time::sleep(TIMERS_FIRED).await;
    assert_eq!(signal.site.launches(), 3, "no start without Restart");

    // The WhatsApp helper: the same process, no new event, still linked.
    whatsapp.drain();
    assert_eq!(
        whatsapp.site.launches(),
        1,
        "the WhatsApp helper kept running"
    );
    assert_eq!(
        whatsapp.seen.len(),
        whatsapp_events,
        "the Signal helper changed nothing for WhatsApp: {:?}",
        &whatsapp.seen[whatsapp_events..]
    );
    whatsapp.send(AdapterCommand::LoadChats {
        protocol: ProtocolId::WhatsApp,
    });
    whatsapp
        .until("ConversationUpsert", |event| {
            matches!(event, AdapterEvent::ConversationUpsert { conversation } if conversation.protocol == ProtocolId::WhatsApp)
        })
        .await;

    // Restart starts the Signal helper again, and the account links again.
    signal.send(AdapterCommand::RestartHelper {
        protocol: ProtocolId::Signal,
    });
    signal.until_helper(HelperState::Running).await;
    signal.until_account(AccountState::Linked).await;
    assert_eq!(signal.site.launches(), 4);
    assert_eq!(whatsapp.site.launches(), 1);
    signal.shut_down().await;
    whatsapp.shut_down().await;
}
