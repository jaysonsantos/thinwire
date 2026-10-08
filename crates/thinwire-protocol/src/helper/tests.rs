//! Tests of the helper adapter against a fake helper (#246).
//!
//! The fake helper is a tokio task behind in-memory pipes. It runs the real
//! [`serve`] loop over a small gated adapter, or it plays a broken helper
//! line by line. So the tests cover the wire protocol from both sides with
//! no AGPL code and no child process.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use thinwire_ipc::{
    AppLine, FrameReader, HelperLine, HelperRefusal, PROTOCOL_VERSION, WireProtocol, write_line,
};
use tokio::io::{AsyncWriteExt, BufReader, DuplexStream};
use tokio::sync::Notify;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tokio::sync::oneshot;

use super::launch::{HelperLauncher, HelperProcess};
use super::serve::{ServeConfig, ServeEnd, serve};
use super::supervisor::{HelperAdapter, HelperSpec, HelperTiming};
use crate::adapter::{
    AccountState, AdapterCommand, AdapterError, AdapterEvent, AdapterStatus, Arrival, ChatMessage,
    Conversation, Delivery, EventTx, HelperFault, HelperState, ProtocolAdapter,
    ProtocolCapabilities, ProtocolId, RedactedPairingSecret, SupportClass, emit_account,
    emit_chat_list_loaded, emit_conversation, emit_history_loaded, emit_message,
    emit_older_history_loaded, emit_send_accepted, emit_send_rejected, emit_status, emit_stopped,
};
use crate::contract::Contract;
use crate::whatsapp::WhatsAppPhoneVault;

const PROTOCOL: ProtocolId = ProtocolId::WhatsApp;
const CHAT: &str = "whatsapp:111@s.whatsapp.net";
const PHONE: &str = "15550100";
/// Size of the in-memory pipes.
const PIPE_BYTES: usize = 64 * 1024;
/// Longest wait for one event in a test.
const EVENT_WAIT: Duration = Duration::from_secs(5);
/// A time with no helper start that proves "no restart".
const NO_START_WAIT: Duration = Duration::from_millis(300);

const CAPS: ProtocolCapabilities = ProtocolCapabilities {
    id: PROTOCOL,
    support: SupportClass::Experimental,
    short_label: "fake helper",
    detail: "A fake helper for tests.",
    official_api: false,
    allows_user_account_automation: false,
    sends_text: true,
    pages_history: true,
};

/// Short waits, so a restart test takes milliseconds.
const TIMING: HelperTiming = HelperTiming {
    hello_wait: Duration::from_secs(2),
    busy_after: Duration::from_millis(50),
    restart_base: Duration::from_millis(20),
    restart_max: Duration::from_millis(100),
    max_failures: 3,
    stable_run: Duration::from_secs(60),
    stop_wait: Duration::from_millis(300),
    kill_wait: Duration::from_secs(2),
};

/// How long the fake process of [`Mode::SlowDeath`] takes to end. Longer
/// than the first restart wait.
const SLOW_DEATH: Duration = Duration::from_millis(200);

// region: fake helper

/// What the fake helper process does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// The real `serve` loop over [`GatedFake`].
    Serve,
    /// `serve`, but the process ends when a pairing starts.
    DieOnBegin,
    /// `Hello` with another protocol version.
    WrongVersion,
    /// `Refused { SessionInUse }`, then the process ends.
    SessionInUse,
    /// `Hello`, then a line that is not in the protocol.
    Garbage,
    /// `Hello`, then no read until the test releases it.
    Stall,
    /// `Hello` and `Ack` lines, but no answer to `Shutdown`.
    IgnoreShutdown,
    /// `Hello`, then the output closes. The process ends [`SLOW_DEATH`]
    /// later, also after a kill.
    SlowDeath,
    /// As [`Mode::SlowDeath`], but the process never ends.
    NeverDies,
    /// The first process ends with no `Hello`. The next ones are
    /// [`Mode::Serve`].
    FirstDiesBeforeHello,
}

/// What the test set up, and what the fake helpers did.
struct Lab {
    installed: AtomicBool,
    mode: Mutex<Mode>,
    /// The fake adapter keeps a send open: no answer.
    hold_sends: AtomicBool,
    launches: AtomicU32,
    /// Helper tasks that ended.
    ended: AtomicU32,
    /// Starts while an older helper process still ran.
    overlaps: AtomicU32,
    /// Ends the helper that runs, like a crash.
    crash: Notify,
    /// Lets a stalled helper read again.
    release: Notify,
    /// The adapter of the last helper saw `shutdown`.
    adapter_shut_down: AtomicBool,
}

impl Lab {
    fn new(mode: Mode) -> Arc<Self> {
        Arc::new(Self {
            installed: AtomicBool::new(true),
            mode: Mutex::new(mode),
            hold_sends: AtomicBool::new(false),
            launches: AtomicU32::new(0),
            ended: AtomicU32::new(0),
            overlaps: AtomicU32::new(0),
            crash: Notify::new(),
            release: Notify::new(),
            adapter_shut_down: AtomicBool::new(false),
        })
    }

    fn set_mode(&self, mode: Mode) {
        *self.mode.lock().expect("mode") = mode;
    }

    fn mode(&self) -> Mode {
        *self.mode.lock().expect("mode")
    }

    fn launches(&self) -> u32 {
        self.launches.load(Ordering::SeqCst)
    }

    fn ended(&self) -> u32 {
        self.ended.load(Ordering::SeqCst)
    }

    fn overlaps(&self) -> u32 {
        self.overlaps.load(Ordering::SeqCst)
    }
}

struct FakeLauncher(Arc<Lab>);

impl HelperLauncher for FakeLauncher {
    fn installed(&self) -> bool {
        self.0.installed.load(Ordering::SeqCst)
    }

    fn launch(&self) -> std::io::Result<HelperProcess> {
        let lab = Arc::clone(&self.0);
        if lab.launches() > lab.ended() {
            lab.overlaps.fetch_add(1, Ordering::SeqCst);
        }
        let mode = lab.mode();
        lab.launches.fetch_add(1, Ordering::SeqCst);
        let (stdin, helper_in) = tokio::io::duplex(PIPE_BYTES);
        let (helper_out, stdout) = tokio::io::duplex(PIPE_BYTES);
        let (exited_tx, exited) = oneshot::channel();
        let (kill, killed) = oneshot::channel::<()>();
        tokio::spawn(async move {
            tokio::select! {
                () = fake_process(Arc::clone(&lab), helper_in, helper_out) => {}
                () = lab.crash.notified() => {}
                _ = killed => {}
            }
            // The pipes of the process are closed here. The process itself
            // can take longer to end.
            match mode {
                Mode::SlowDeath => tokio::time::sleep(SLOW_DEATH).await,
                Mode::NeverDies => std::future::pending::<()>().await,
                _ => {}
            }
            lab.ended.fetch_add(1, Ordering::SeqCst);
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

async fn fake_process(lab: Arc<Lab>, stdin: DuplexStream, mut stdout: DuplexStream) {
    let hello = |protocol_version| HelperLine::Hello {
        protocol_version,
        helper_version: "fake".into(),
        protocols: vec![WireProtocol::WhatsApp],
    };
    match lab.mode() {
        Mode::FirstDiesBeforeHello if lab.launches() == 1 => {}
        Mode::Serve | Mode::DieOnBegin | Mode::FirstDiesBeforeHello => {
            let phone = Arc::new(WhatsAppPhoneVault::new());
            let adapter = GatedFake::new(Arc::clone(&lab), Arc::clone(&phone));
            let config = ServeConfig {
                helper_version: "fake",
                phone: Some(phone),
            };
            let _ = serve(Box::new(adapter), config, stdin, stdout).await;
        }
        Mode::WrongVersion => {
            let _ = write_line(&mut stdout, &hello(PROTOCOL_VERSION + 1)).await;
            std::future::pending::<()>().await;
        }
        Mode::SessionInUse => {
            let line = HelperLine::Refused {
                reason: HelperRefusal::SessionInUse,
            };
            let _ = write_line(&mut stdout, &line).await;
        }
        Mode::Garbage => {
            let _ = write_line(&mut stdout, &hello(PROTOCOL_VERSION)).await;
            let _ = stdout.write_all(b"this is not a wire line\n").await;
            std::future::pending::<()>().await;
        }
        // The output closes at the return. The launcher keeps the process.
        Mode::SlowDeath | Mode::NeverDies => {
            let _ = write_line(&mut stdout, &hello(PROTOCOL_VERSION)).await;
        }
        Mode::Stall | Mode::IgnoreShutdown => {
            let _ = write_line(&mut stdout, &hello(PROTOCOL_VERSION)).await;
            if lab.mode() == Mode::Stall {
                lab.release.notified().await;
            }
            let mut frames = FrameReader::new(BufReader::new(stdin));
            while let Ok(Some(line)) = frames.next::<AppLine>().await {
                if let AppLine::Request { id, .. } = line {
                    let _ = write_line(&mut stdout, &HelperLine::Ack { id }).await;
                }
            }
        }
    }
}

/// The adapter inside the fake helper: a gate, then a pairing that links at
/// once, then one chat.
struct GatedFake {
    lab: Arc<Lab>,
    phone: Arc<WhatsAppPhoneVault>,
    acknowledged: bool,
    linked: bool,
}

impl GatedFake {
    fn new(lab: Arc<Lab>, phone: Arc<WhatsAppPhoneVault>) -> Self {
        Self {
            lab,
            phone,
            acknowledged: false,
            linked: false,
        }
    }

    fn chat() -> Conversation {
        Conversation {
            protocol: PROTOCOL,
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
        }
    }

    fn message(id: &str, body: &str, outbound: bool) -> ChatMessage {
        ChatMessage {
            protocol: PROTOCOL,
            conversation_id: CHAT.into(),
            id: id.into(),
            sender: "Ana".into(),
            body: body.into(),
            outbound,
            delivery: Delivery::Sent,
            sent_at: 1_790_000_000,
            arrival: Arrival::History,
        }
    }

    fn failed(events: &EventTx, conversation_id: Option<String>) {
        let _ = events.send(AdapterEvent::CommandFailed {
            protocol: PROTOCOL,
            conversation_id,
            detail: "not available in the fake helper".into(),
        });
    }
}

impl ProtocolAdapter for GatedFake {
    fn id(&self) -> ProtocolId {
        PROTOCOL
    }

    fn capabilities(&self) -> ProtocolCapabilities {
        CAPS
    }

    fn start(&mut self, events: EventTx) {
        emit_status(&events, PROTOCOL, AdapterStatus::Stubbed, "fake helper up");
    }

    fn shutdown(&mut self, events: &EventTx) {
        self.lab.adapter_shut_down.store(true, Ordering::SeqCst);
        emit_stopped(events, PROTOCOL);
    }

    fn handle(&mut self, command: AdapterCommand, events: &EventTx) -> Result<(), AdapterError> {
        match command {
            AdapterCommand::WhatsAppAcknowledgeRisk => self.acknowledged = true,
            AdapterCommand::WhatsAppBeginLink { generation } => {
                if !self.acknowledged {
                    return Err(AdapterError::Refused {
                        protocol: PROTOCOL,
                        reason: "the gate was not accepted",
                    });
                }
                if self.lab.mode() == Mode::DieOnBegin {
                    self.lab.crash.notify_one();
                    return Ok(());
                }
                // The QR names the phone, so a test sees what crossed.
                let phone = self.phone.phone().unwrap_or_else(|| "none".into());
                let _ = events.send(AdapterEvent::WhatsAppQr {
                    code: RedactedPairingSecret::new(format!("qr:{phone}")),
                    generation,
                });
                emit_account(events, PROTOCOL, AccountState::Linking);
                self.linked = true;
                emit_account(events, PROTOCOL, AccountState::Linked);
                emit_status(events, PROTOCOL, AdapterStatus::Ready, "linked");
                emit_conversation(events, Self::chat());
                emit_chat_list_loaded(events, PROTOCOL);
            }
            AdapterCommand::LoadChats { .. } => {
                if self.linked {
                    emit_conversation(events, Self::chat());
                } else {
                    Self::failed(events, None);
                }
                emit_chat_list_loaded(events, PROTOCOL);
            }
            AdapterCommand::OpenChat {
                conversation_id, ..
            } => {
                if self.linked && conversation_id == CHAT {
                    emit_message(events, Self::message("m1", "hello", false));
                } else {
                    Self::failed(events, Some(conversation_id.clone()));
                }
                emit_history_loaded(events, PROTOCOL, conversation_id);
            }
            AdapterCommand::SendText {
                conversation_id,
                body,
                request,
                ..
            } => {
                if self.lab.hold_sends.load(Ordering::SeqCst) {
                    return Ok(());
                }
                if !self.linked {
                    emit_send_rejected(events, PROTOCOL, conversation_id, request);
                    return Ok(());
                }
                emit_message(
                    events,
                    Self::message(&format!("sent:{request}"), &body, true),
                );
                emit_send_accepted(events, PROTOCOL, conversation_id, request);
            }
            AdapterCommand::ResendMessage {
                conversation_id,
                request,
                ..
            } => emit_send_rejected(events, PROTOCOL, conversation_id, request),
            AdapterCommand::LoadOlderMessages {
                conversation_id,
                before_message_id,
                ..
            } => emit_older_history_loaded(
                events,
                PROTOCOL,
                conversation_id,
                before_message_id,
                false,
                None,
            ),
            AdapterCommand::Disconnect { .. } | AdapterCommand::WhatsAppCancelLink => {
                self.acknowledged = false;
                self.linked = false;
                emit_account(events, PROTOCOL, AccountState::Unlinked);
            }
            _ => {}
        }
        Ok(())
    }
}

// endregion: fake helper

// region: probe

/// A helper adapter on its own event channel.
struct Probe {
    adapter: HelperAdapter,
    lab: Arc<Lab>,
    phone: Arc<WhatsAppPhoneVault>,
    tx: EventTx,
    rx: UnboundedReceiver<AdapterEvent>,
    seen: Vec<AdapterEvent>,
}

fn helper_adapter(lab: &Arc<Lab>, phone: &Arc<WhatsAppPhoneVault>) -> HelperAdapter {
    HelperAdapter::new(
        HelperSpec {
            protocol: PROTOCOL,
            capabilities: CAPS,
        },
        Arc::new(FakeLauncher(Arc::clone(lab))),
        Some(Arc::clone(phone)),
    )
    .with_timing(TIMING)
}

impl Probe {
    fn start(mode: Mode) -> Self {
        Self::start_with(Lab::new(mode))
    }

    fn start_with(lab: Arc<Lab>) -> Self {
        let phone = Arc::new(WhatsAppPhoneVault::new());
        let mut adapter = helper_adapter(&lab, &phone);
        let (tx, rx) = unbounded_channel();
        adapter.start(tx.clone());
        Self {
            adapter,
            lab,
            phone,
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

    /// Accept the gate and start pairing number `generation`.
    fn pair(&mut self, generation: u64) {
        self.send(AdapterCommand::WhatsAppAcknowledgeRisk);
        self.send(AdapterCommand::WhatsAppBeginLink { generation });
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
        self.until(&format!("Helper {state:?}"), |event| {
            *event
                == AdapterEvent::Helper {
                    protocol: PROTOCOL,
                    state,
                }
        })
        .await;
    }

    async fn until_account(&mut self, state: AccountState) {
        self.until(&format!("Account {state:?}"), |event| {
            *event
                == AdapterEvent::Account {
                    protocol: PROTOCOL,
                    state,
                }
        })
        .await;
    }

    /// Read events until none comes for a short time.
    async fn settle(&mut self) {
        while let Ok(Some(event)) =
            tokio::time::timeout(Duration::from_millis(100), self.rx.recv()).await
        {
            self.seen.push(event);
        }
    }

    fn saw(&self, test: impl Fn(&AdapterEvent) -> bool) -> bool {
        self.seen.iter().any(test)
    }

    fn helper_states(&self) -> Vec<HelperState> {
        self.seen
            .iter()
            .filter_map(|event| match event {
                AdapterEvent::Helper { state, .. } => Some(*state),
                _ => None,
            })
            .collect()
    }
}

// endregion: probe

// region: round trip

/// The adapter contract of ADR 0010 holds through the pipe: the kit drives
/// the MIT helper adapter, and the answers come from the fake helper.
#[tokio::test]
async fn the_adapter_contract_holds_through_the_pipe() {
    let lab = Lab::new(Mode::Serve);
    let phone = Arc::new(WhatsAppPhoneVault::new());
    let mut kit = Contract::new(Box::new(helper_adapter(&lab, &phone)));
    assert!(!kit.send(AdapterCommand::WhatsAppAcknowledgeRisk));
    assert!(!kit.send(AdapterCommand::WhatsAppBeginLink { generation: 1 }));
    kit.linked().await;
    kit.run_all().await;
    assert_eq!(lab.launches(), 1, "one helper for the whole session");
    assert!(
        lab.adapter_shut_down.load(Ordering::SeqCst),
        "Shutdown reached the adapter in the helper"
    );
}

/// The phone for a pair code crosses the pipe once, with the pairing start
/// (ADR 0012 "Secrets on the wire"). A pairing with no phone carries none.
#[tokio::test]
async fn the_phone_crosses_only_with_the_pairing_start() {
    let mut probe = Probe::start(Mode::Serve);
    probe.phone.set_phone(PHONE);
    probe.pair(7);
    let qr = probe
        .until("QR", |event| {
            matches!(event, AdapterEvent::WhatsAppQr { .. })
        })
        .await;
    let AdapterEvent::WhatsAppQr { code, generation } = qr else {
        unreachable!("matched above");
    };
    assert_eq!(code.reveal(), format!("qr:{PHONE}"));
    assert_eq!(generation, 7, "the pairing id survives the pipe (rule 8)");

    probe.phone.clear();
    probe.send(AdapterCommand::WhatsAppBeginLink { generation: 8 });
    let qr = probe
        .until("second QR", |event| {
            matches!(event, AdapterEvent::WhatsAppQr { generation: 8, .. })
        })
        .await;
    let AdapterEvent::WhatsAppQr { code, .. } = qr else {
        unreachable!("matched above");
    };
    assert_eq!(code.reveal(), "qr:none", "the old phone is not used again");
}

/// PR #256 review: the phone crosses the pipe one time. A restart sends the
/// pairing again, so the account links again from its session store, but
/// with no phone.
#[tokio::test]
async fn a_restart_does_not_send_the_phone_again() {
    let mut probe = Probe::start(Mode::Serve);
    probe.phone.set_phone(PHONE);
    probe.pair(1);
    probe
        .until("QR with the phone", |event| {
            matches!(event, AdapterEvent::WhatsAppQr { code, generation: 1 }
                if code.reveal() == format!("qr:{PHONE}"))
        })
        .await;
    probe.until_account(AccountState::Linked).await;

    // The phone is still in the vault of the app.
    assert_eq!(probe.phone.phone().as_deref(), Some(PHONE));
    probe.lab.crash.notify_one();
    probe.until_helper(HelperState::Restarting).await;
    let replayed = probe
        .until("QR of the replayed pairing", |event| {
            matches!(event, AdapterEvent::WhatsAppQr { generation: 1, .. })
        })
        .await;
    let AdapterEvent::WhatsAppQr { code, .. } = replayed else {
        unreachable!("matched above");
    };
    assert_eq!(code.reveal(), "qr:none", "the replay carries no phone");
    probe.until_account(AccountState::Linked).await;
}

/// PR #256 review: a pairing start that waited for `Hello` never crossed
/// the pipe. If that helper ends, the next one gets the pairing with its
/// phone. A pair-code pairing does not become a QR pairing without a word.
#[tokio::test]
async fn a_pairing_that_the_helper_never_got_keeps_its_phone() {
    let mut probe = Probe::start(Mode::FirstDiesBeforeHello);
    probe.phone.set_phone(PHONE);
    probe.pair(1);
    probe.until_helper(HelperState::Restarting).await;
    let qr = probe
        .until("QR from the second helper", |event| {
            matches!(event, AdapterEvent::WhatsAppQr { generation: 1, .. })
        })
        .await;
    let AdapterEvent::WhatsAppQr { code, .. } = qr else {
        unreachable!("matched above");
    };
    assert_eq!(code.reveal(), format!("qr:{PHONE}"));
    probe.until_account(AccountState::Linked).await;
    assert_eq!(probe.lab.launches(), 2);

    // That start crossed the pipe. A later restart sends no phone.
    probe.lab.crash.notify_one();
    probe.until_helper(HelperState::Restarting).await;
    let replayed = probe
        .until("QR of the replayed pairing", |event| {
            matches!(event, AdapterEvent::WhatsAppQr { generation: 1, .. })
        })
        .await;
    let AdapterEvent::WhatsAppQr { code, .. } = replayed else {
        unreachable!("matched above");
    };
    assert_eq!(code.reveal(), "qr:none");
}

/// A pairing start of the user while no helper runs starts the helper. It
/// is a new start, so it keeps its phone, and it takes the place of the old
/// pairing in the replay.
#[tokio::test]
async fn a_pairing_start_that_wakes_the_helper_keeps_its_phone() {
    let mut probe = Probe::start(Mode::Serve);
    probe.pair(1);
    probe.until_account(AccountState::Linked).await;
    // The next helper has another wire version: the supervisor stops.
    probe.lab.set_mode(Mode::WrongVersion);
    probe.lab.crash.notify_one();
    probe
        .until_helper(HelperState::Stopped(HelperFault::VersionMismatch))
        .await;
    probe.settle().await;
    let seen = probe.seen.len();

    probe.lab.set_mode(Mode::Serve);
    probe.phone.set_phone(PHONE);
    probe.send(AdapterCommand::WhatsAppBeginLink { generation: 2 });
    let qr = probe
        .until("QR of the new pairing", |event| {
            matches!(event, AdapterEvent::WhatsAppQr { generation: 2, .. })
        })
        .await;
    let AdapterEvent::WhatsAppQr { code, .. } = qr else {
        unreachable!("matched above");
    };
    assert_eq!(code.reveal(), format!("qr:{PHONE}"));
    probe.until_account(AccountState::Linked).await;
    probe.settle().await;
    assert!(
        !probe.seen[seen..]
            .iter()
            .any(|event| matches!(event, AdapterEvent::WhatsAppQr { generation: 1, .. })),
        "the old pairing is not sent again next to the new one"
    );
}

/// ADR 0013: the helper starts at the gate, not at app start. Before that,
/// every command of the shell still ends (ADR 0010 rules 4, 5 and 9).
#[tokio::test]
async fn no_helper_starts_before_the_gate_and_every_command_ends() {
    let mut probe = Probe::start(Mode::Serve);
    for command in [
        AdapterCommand::Connect { protocol: PROTOCOL },
        AdapterCommand::LoadChats { protocol: PROTOCOL },
        AdapterCommand::OpenChat {
            protocol: PROTOCOL,
            conversation_id: CHAT.into(),
        },
        AdapterCommand::SendText {
            protocol: PROTOCOL,
            conversation_id: CHAT.into(),
            body: "hello".into(),
            request: 7,
        },
        AdapterCommand::LoadOlderMessages {
            protocol: PROTOCOL,
            conversation_id: CHAT.into(),
            before_message_id: "m1".into(),
        },
    ] {
        probe.send(command);
    }
    probe
        .until("OlderHistoryLoaded", |event| {
            matches!(event, AdapterEvent::OlderHistoryLoaded { more: false, .. })
        })
        .await;
    assert_eq!(probe.lab.launches(), 0, "no helper process before the gate");
    assert!(probe.saw(|event| *event == AdapterEvent::ChatListLoaded { protocol: PROTOCOL }));
    assert!(probe.saw(|event| *event
        == AdapterEvent::HistoryLoaded {
            protocol: PROTOCOL,
            conversation_id: CHAT.into(),
        }));
    assert!(probe.saw(|event| *event
        == AdapterEvent::SendRejected {
            protocol: PROTOCOL,
            conversation_id: CHAT.into(),
            request: 7,
        }));
    let failed = probe
        .seen
        .iter()
        .filter(|event| matches!(event, AdapterEvent::CommandFailed { .. }))
        .count();
    assert_eq!(failed, 2, "LoadChats and OpenChat fail as commands");
    assert!(probe.helper_states().is_empty(), "no helper state to show");
}

/// A command that no helper takes is an error from `handle`, as in every
/// other adapter.
#[tokio::test]
async fn a_command_of_another_protocol_is_refused() {
    let mut probe = Probe::start(Mode::Serve);
    let refused = probe.adapter.handle(
        AdapterCommand::LoadChats {
            protocol: ProtocolId::Telegram,
        },
        &probe.tx,
    );
    assert!(matches!(refused, Err(AdapterError::Unavailable { .. })));
}

// endregion: round trip

// region: failure

/// #246 "Helper crash test". The helper dies: the account goes to Linking
/// and the row to Restarting. The supervisor starts a new helper, sends the
/// gate and the pairing again, and the account links again.
#[tokio::test]
async fn a_helper_that_dies_starts_again_and_the_account_links_again() {
    let mut probe = Probe::start(Mode::Serve);
    probe.pair(1);
    probe.until_account(AccountState::Linked).await;
    assert_eq!(
        probe.helper_states(),
        [HelperState::Busy, HelperState::Running]
    );

    probe.lab.crash.notify_one();
    probe.until_account(AccountState::Linking).await;
    probe.until_helper(HelperState::Restarting).await;
    probe
        .until("Notice", |event| {
            matches!(event, AdapterEvent::Notice { text, .. }
                if text == "The WhatsApp helper stopped. thinwire starts it again.")
        })
        .await;
    probe.until_helper(HelperState::Running).await;
    probe.until_account(AccountState::Linked).await;
    assert_eq!(probe.lab.launches(), 2);
    assert_eq!(
        probe.lab.ended(),
        1,
        "the first helper ended before the next one started"
    );
    assert_eq!(probe.lab.overlaps(), 0, "one process at a time");
    // The new helper answers.
    probe.send(AdapterCommand::LoadChats { protocol: PROTOCOL });
    probe
        .until("ChatListLoaded", |event| {
            *event == AdapterEvent::ChatListLoaded { protocol: PROTOCOL }
        })
        .await;
}

/// ADR 0012 "Failure": every open send and retry gets `SendRejected` when
/// the helper ends (ADR 0010 rule 4). A send that waits for `Hello` counts.
#[tokio::test]
async fn open_sends_are_rejected_when_the_helper_dies() {
    let mut probe = Probe::start(Mode::Serve);
    probe.lab.hold_sends.store(true, Ordering::SeqCst);
    probe.pair(1);
    probe.until_account(AccountState::Linked).await;
    for request in [41, 42] {
        probe.send(AdapterCommand::SendText {
            protocol: PROTOCOL,
            conversation_id: CHAT.into(),
            body: "hello".into(),
            request,
        });
    }
    probe.settle().await;
    probe.lab.crash.notify_one();
    probe.until_helper(HelperState::Restarting).await;
    probe.settle().await;
    for request in [41, 42] {
        assert!(
            probe.saw(|event| *event
                == AdapterEvent::SendRejected {
                    protocol: PROTOCOL,
                    conversation_id: CHAT.into(),
                    request,
                }),
            "send {request} has no answer"
        );
    }
}

/// ADR 0013 decision 4: after the last failed start the supervisor stops.
/// The row gets `Stopped`, the account unlinks, and no helper starts until
/// the user clicks Restart. Restart starts the helper. The pairing did not
/// link, so it ended: the user starts it again.
#[tokio::test]
async fn after_the_last_failure_the_helper_stays_stopped_until_restart() {
    let mut probe = Probe::start(Mode::DieOnBegin);
    probe.pair(1);
    probe
        .until_helper(HelperState::Stopped(HelperFault::Crashed))
        .await;
    assert_eq!(probe.lab.launches(), TIMING.max_failures);
    probe
        .until("the stopped-helper status", |event| {
            matches!(event, AdapterEvent::Status {
                status: AdapterStatus::Error,
                detail,
                ..
            } if detail == "The WhatsApp helper stopped. Click Restart on the WhatsApp account.")
        })
        .await;
    tokio::time::sleep(NO_START_WAIT).await;
    assert_eq!(
        probe.lab.launches(),
        TIMING.max_failures,
        "no start without Restart"
    );

    probe.lab.set_mode(Mode::Serve);
    probe.send(AdapterCommand::RestartHelper { protocol: PROTOCOL });
    probe.until_helper(HelperState::Running).await;
    assert_eq!(probe.lab.launches(), TIMING.max_failures + 1);
    probe.send(AdapterCommand::WhatsAppBeginLink { generation: 2 });
    probe.until_account(AccountState::Linked).await;
    assert_eq!(probe.lab.launches(), TIMING.max_failures + 1);
}

/// #246 UX: Restart starts the helper again, and an account that was linked
/// links again from its session store.
#[tokio::test]
async fn restart_links_a_linked_account_again() {
    let mut probe = Probe::start(Mode::Serve);
    probe.pair(1);
    probe.until_account(AccountState::Linked).await;
    // The next helper has another wire version: the supervisor stops.
    probe.lab.set_mode(Mode::WrongVersion);
    probe.lab.crash.notify_one();
    probe
        .until_helper(HelperState::Stopped(HelperFault::VersionMismatch))
        .await;

    probe.lab.set_mode(Mode::Serve);
    probe.send(AdapterCommand::RestartHelper { protocol: PROTOCOL });
    probe.until_helper(HelperState::Running).await;
    probe.until_account(AccountState::Linked).await;
}

/// PR #256 review: a pairing that stopped for good before it linked is over.
/// Restart starts the helper, but it does not send that pairing again with
/// no phone: a pair-code pairing never comes back as a QR pairing. The user
/// starts the pairing again, with the phone.
#[tokio::test]
async fn restart_does_not_send_a_pairing_that_did_not_link() {
    let mut probe = Probe::start(Mode::WrongVersion);
    probe.phone.set_phone(PHONE);
    probe.pair(1);
    probe
        .until_helper(HelperState::Stopped(HelperFault::VersionMismatch))
        .await;

    probe.lab.set_mode(Mode::Serve);
    probe.send(AdapterCommand::RestartHelper { protocol: PROTOCOL });
    probe.until_helper(HelperState::Running).await;
    probe.settle().await;
    assert!(
        !probe.saw(|event| matches!(event, AdapterEvent::WhatsAppQr { .. })),
        "no pairing without a new start of the user"
    );
    assert!(!probe.saw(|event| *event
        == AdapterEvent::Account {
            protocol: PROTOCOL,
            state: AccountState::Linked,
        }));

    probe.send(AdapterCommand::WhatsAppBeginLink { generation: 2 });
    let qr = probe
        .until("QR of the new pairing", |event| {
            matches!(event, AdapterEvent::WhatsAppQr { generation: 2, .. })
        })
        .await;
    let AdapterEvent::WhatsAppQr { code, .. } = qr else {
        unreachable!("matched above");
    };
    assert_eq!(code.reveal(), format!("qr:{PHONE}"));
}

/// A linked account whose helper stops for good is unlinked, so the shell
/// does not show rows that no client serves.
#[tokio::test]
async fn a_linked_account_unlinks_when_the_helper_stops_for_good() {
    let mut probe = Probe::start(Mode::Serve);
    probe.pair(1);
    probe.until_account(AccountState::Linked).await;
    // From now on every new helper dies in the replay.
    probe.lab.set_mode(Mode::DieOnBegin);
    probe.lab.crash.notify_one();
    probe.until_account(AccountState::Linking).await;
    probe.until_account(AccountState::Unlinked).await;
    probe
        .until_helper(HelperState::Stopped(HelperFault::Crashed))
        .await;
}

/// A helper line that is not in the protocol ends that helper (ADR 0012
/// "Failure"). It counts as a failed run.
#[tokio::test]
async fn a_bad_line_from_the_helper_counts_as_a_failure() {
    let mut probe = Probe::start(Mode::Garbage);
    probe.pair(1);
    probe.until_helper(HelperState::Restarting).await;
    probe
        .until_helper(HelperState::Stopped(HelperFault::Crashed))
        .await;
    assert_eq!(probe.lab.launches(), TIMING.max_failures);
    assert_eq!(
        probe.lab.ended(),
        TIMING.max_failures,
        "each broken helper was ended"
    );
}

/// PR #256 review: a helper that closes its output can still run and hold
/// the lock of the session store. The next helper starts only after the old
/// process ended. Else it finds the lock, and a crash that the app can
/// repair becomes "Helper stopped."
#[tokio::test]
async fn a_new_helper_starts_only_after_the_old_process_ended() {
    let mut probe = Probe::start(Mode::SlowDeath);
    probe.pair(1);
    probe.until_helper(HelperState::Restarting).await;
    assert_eq!(probe.lab.ended(), 0, "the old process still runs");
    // The restart wait is shorter than the death of the old process.
    tokio::time::sleep(TIMING.restart_base * 3).await;
    assert_eq!(probe.lab.launches(), 1, "no second process next to it");

    probe.lab.set_mode(Mode::Serve);
    probe.until_helper(HelperState::Running).await;
    probe.until_account(AccountState::Linked).await;
    assert_eq!(probe.lab.launches(), 2);
    assert_eq!(probe.lab.overlaps(), 0, "one process at a time");
}

/// A helper process that does not end after its kill blocks the next start.
/// The supervisor then stops and says so. It never starts a second process
/// on the same session store.
#[tokio::test]
async fn a_helper_process_that_never_ends_stops_the_restart() {
    let mut probe = Probe::start(Mode::NeverDies);
    probe.pair(1);
    probe.until_helper(HelperState::Restarting).await;
    probe
        .until_helper(HelperState::Stopped(HelperFault::StillRunning))
        .await;
    assert_eq!(probe.lab.launches(), 1);
    assert_eq!(probe.lab.overlaps(), 0);
}

/// The user cancelled the link: a helper that dies after that does not start
/// again, and the row shows no fault.
#[tokio::test]
async fn a_helper_with_no_link_to_keep_does_not_restart() {
    let mut probe = Probe::start(Mode::Serve);
    probe.pair(1);
    probe.until_account(AccountState::Linked).await;
    probe.send(AdapterCommand::WhatsAppCancelLink);
    probe.until_account(AccountState::Unlinked).await;
    probe.lab.crash.notify_one();
    probe.until_helper(HelperState::Idle).await;
    tokio::time::sleep(NO_START_WAIT).await;
    assert_eq!(probe.lab.launches(), 1);
}

// endregion: failure

// region: start

/// ADR 0013 decision 6: a missing helper does not hide the protocol. The
/// adapter reports `Missing`, starts nothing, and names what to do.
#[tokio::test]
async fn a_missing_helper_is_reported_and_never_started() {
    let lab = Lab::new(Mode::Serve);
    lab.installed.store(false, Ordering::SeqCst);
    let mut probe = Probe::start_with(lab);
    probe.until_helper(HelperState::Missing).await;
    probe.pair(1);
    probe
        .until("the missing-helper status", |event| {
            matches!(event, AdapterEvent::Status {
                status: AdapterStatus::Error,
                detail,
                ..
            } if detail == "WhatsApp helper missing. Reinstall thinwire.")
        })
        .await;
    assert_eq!(probe.lab.launches(), 0);

    // The user installed it while the app runs: the next gate starts it.
    probe.lab.installed.store(true, Ordering::SeqCst);
    probe.pair(2);
    probe.until_account(AccountState::Linked).await;
    assert_eq!(probe.lab.launches(), 1);
}

/// A helper of another wire version is not used, and it does not start
/// again by itself: a restart cannot change its version.
#[tokio::test]
async fn a_helper_with_another_wire_version_is_refused() {
    let mut probe = Probe::start(Mode::WrongVersion);
    probe.pair(1);
    probe
        .until_helper(HelperState::Stopped(HelperFault::VersionMismatch))
        .await;
    tokio::time::sleep(NO_START_WAIT).await;
    assert_eq!(probe.lab.launches(), 1);
    assert_eq!(probe.lab.ended(), 1, "the helper was ended");
}

/// Two helpers on one session store break it. The second one refuses to
/// run, and the supervisor does not start it again by itself.
#[tokio::test]
async fn a_helper_whose_session_is_in_use_does_not_restart() {
    let mut probe = Probe::start(Mode::SessionInUse);
    probe.pair(1);
    probe
        .until_helper(HelperState::Stopped(HelperFault::SessionInUse))
        .await;
    tokio::time::sleep(NO_START_WAIT).await;
    assert_eq!(probe.lab.launches(), 1);
}

/// #246 UX: a slow helper shows a busy row, and the caller never waits.
/// `handle` returns at once while the helper reads nothing.
#[tokio::test]
async fn a_slow_helper_makes_the_row_busy_and_no_caller_waits() {
    let mut probe = Probe::start(Mode::Stall);
    let before = std::time::Instant::now();
    probe.pair(1);
    for _ in 0..100 {
        probe.send(AdapterCommand::LoadChats { protocol: PROTOCOL });
    }
    assert!(
        before.elapsed() < TIMING.busy_after,
        "handle waited on the pipe: {:?}",
        before.elapsed()
    );
    // Busy at the start, Running after Hello, then Busy: no Ack in time.
    probe.until_helper(HelperState::Running).await;
    probe.until_helper(HelperState::Busy).await;
    probe.lab.release.notify_one();
    probe.until_helper(HelperState::Running).await;
    assert_eq!(probe.lab.launches(), 1, "slow is not a failure");
}

// endregion: start

// region: stop

/// The app closes: the helper gets `Shutdown`, its adapter stops, and the
/// shell gets one `Stopped`.
#[tokio::test]
async fn shutdown_stops_the_helper_and_reports_stopped_once() {
    let mut probe = Probe::start(Mode::Serve);
    probe.pair(1);
    probe.until_account(AccountState::Linked).await;
    probe.adapter.shutdown(&probe.tx);
    probe
        .until("Stopped", |event| {
            *event == AdapterEvent::Stopped { protocol: PROTOCOL }
        })
        .await;
    probe.settle().await;
    assert!(probe.lab.adapter_shut_down.load(Ordering::SeqCst));
    assert_eq!(probe.lab.ended(), 1, "the helper process ended");
    let stopped = probe
        .seen
        .iter()
        .filter(|event| matches!(event, AdapterEvent::Stopped { .. }))
        .count();
    assert_eq!(stopped, 1);
    assert_eq!(probe.lab.launches(), 1, "no restart after Shutdown");
}

/// With no helper process, `Stopped` comes at once.
#[tokio::test]
async fn shutdown_with_no_helper_reports_stopped_at_once() {
    let mut probe = Probe::start(Mode::Serve);
    probe.adapter.shutdown(&probe.tx);
    probe
        .until("Stopped", |event| {
            *event == AdapterEvent::Stopped { protocol: PROTOCOL }
        })
        .await;
    assert_eq!(probe.lab.launches(), 0);
}

/// A helper that does not confirm `Shutdown` in time is ended. Then no
/// client runs, so `Stopped` is still true (#44).
#[tokio::test]
async fn a_helper_that_does_not_confirm_shutdown_is_ended() {
    let mut probe = Probe::start(Mode::IgnoreShutdown);
    probe.pair(1);
    probe.until_helper(HelperState::Running).await;
    let before = std::time::Instant::now();
    probe.adapter.shutdown(&probe.tx);
    probe
        .until("Stopped", |event| {
            *event == AdapterEvent::Stopped { protocol: PROTOCOL }
        })
        .await;
    assert!(before.elapsed() >= TIMING.stop_wait, "it waited first");
    probe.settle().await;
    assert_eq!(probe.lab.ended(), 1, "the helper process was ended");
}

/// The adapter is dropped with no `Shutdown` (the app exits another way):
/// the helper still ends.
#[tokio::test]
async fn dropping_the_adapter_ends_the_helper() {
    let mut probe = Probe::start(Mode::Serve);
    probe.pair(1);
    probe.until_account(AccountState::Linked).await;
    let lab = Arc::clone(&probe.lab);
    drop(probe);
    tokio::time::timeout(EVENT_WAIT, async {
        while lab.ended() == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the helper ended");
}

// endregion: stop

// region: serve

/// The helper side on its own: a pipe to `serve` over the gated fake.
struct Served {
    to_helper: DuplexStream,
    from_helper: FrameReader<BufReader<DuplexStream>>,
    task: tokio::task::JoinHandle<ServeEnd>,
    lab: Arc<Lab>,
}

impl Served {
    fn start() -> Self {
        let lab = Lab::new(Mode::Serve);
        let (to_helper, helper_in) = tokio::io::duplex(PIPE_BYTES);
        let (helper_out, from_helper) = tokio::io::duplex(PIPE_BYTES);
        let phone = Arc::new(WhatsAppPhoneVault::new());
        let adapter = GatedFake::new(Arc::clone(&lab), Arc::clone(&phone));
        let config = ServeConfig {
            helper_version: "fake",
            phone: Some(phone),
        };
        let task = tokio::spawn(serve(Box::new(adapter), config, helper_in, helper_out));
        Self {
            to_helper,
            from_helper: FrameReader::new(BufReader::new(from_helper)),
            task,
            lab,
        }
    }

    async fn next(&mut self) -> Option<HelperLine> {
        tokio::time::timeout(EVENT_WAIT, self.from_helper.next::<HelperLine>())
            .await
            .expect("a line in time")
            .expect("a wire line")
    }

    async fn end(self) -> ServeEnd {
        tokio::time::timeout(EVENT_WAIT, self.task)
            .await
            .expect("serve ended")
            .expect("serve did not panic")
    }
}

#[tokio::test]
async fn serve_says_hello_first_and_acks_each_request() {
    let mut served = Served::start();
    assert_eq!(
        served.next().await,
        Some(HelperLine::Hello {
            protocol_version: PROTOCOL_VERSION,
            helper_version: "fake".into(),
            protocols: vec![WireProtocol::WhatsApp],
        })
    );
    let request = AppLine::Request {
        id: 5,
        protocol: WireProtocol::WhatsApp,
        command: thinwire_ipc::WireCommand::LoadChats,
    };
    write_line(&mut served.to_helper, &request)
        .await
        .expect("request");
    let mut lines = Vec::new();
    while !lines.contains(&HelperLine::Ack { id: 5 }) {
        lines.push(served.next().await.expect("a line"));
    }
    write_line(&mut served.to_helper, &AppLine::Shutdown)
        .await
        .expect("shutdown");
    loop {
        match served.next().await {
            Some(HelperLine::Stopped {
                protocol: WireProtocol::WhatsApp,
            }) => break,
            Some(_) => {}
            None => panic!("no Stopped line before the end of the stream"),
        }
    }
    assert!(matches!(served.end().await, ServeEnd::Shutdown));
}

/// ADR 0012 section 4: the helper closes on a bad line, and its adapter
/// shuts down first.
#[tokio::test]
async fn serve_closes_on_a_bad_line() {
    let mut served = Served::start();
    served
        .to_helper
        .write_all(b"{\"type\":\"request\",\"id\":1}\n")
        .await
        .expect("write");
    let lab = Arc::clone(&served.lab);
    assert!(matches!(served.end().await, ServeEnd::BadLine(_)));
    assert!(lab.adapter_shut_down.load(Ordering::SeqCst));
}

/// A request that names another protocol is a bad line: this helper carries
/// WhatsApp only.
#[tokio::test]
async fn serve_closes_on_a_request_for_another_protocol() {
    let mut served = Served::start();
    let request = AppLine::Request {
        id: 1,
        protocol: WireProtocol::Signal,
        command: thinwire_ipc::WireCommand::Connect,
    };
    write_line(&mut served.to_helper, &request)
        .await
        .expect("request");
    assert!(matches!(served.end().await, ServeEnd::BadLine(_)));
}

/// The app is gone (its end of the pipe closed): the helper shuts its
/// adapter down and ends. It does not stay as an orphan process.
#[tokio::test]
async fn serve_ends_when_the_app_closes_the_pipe() {
    let served = Served::start();
    let Served {
        to_helper,
        from_helper,
        task,
        lab,
    } = served;
    drop(to_helper);
    let end = tokio::time::timeout(EVENT_WAIT, task)
        .await
        .expect("serve ended")
        .expect("serve did not panic");
    assert!(matches!(end, ServeEnd::AppGone));
    assert!(lab.adapter_shut_down.load(Ordering::SeqCst));
    drop(from_helper);
}

/// An error from `handle` comes back as a `Status` event, as the app's host
/// reports it.
#[tokio::test]
async fn serve_reports_an_error_from_handle_as_a_status() {
    let mut probe = Probe::start(Mode::Serve);
    // No gate: the fake refuses the pairing in `handle`.
    probe.send(AdapterCommand::WhatsAppBeginLink { generation: 1 });
    let status = probe
        .until("Refused status", |event| {
            matches!(
                event,
                AdapterEvent::Status {
                    status: AdapterStatus::Refused,
                    ..
                }
            )
        })
        .await;
    let AdapterEvent::Status { detail, .. } = status else {
        unreachable!("matched above");
    };
    assert_eq!(detail, "WhatsApp refused: the gate was not accepted");
}

// endregion: serve
