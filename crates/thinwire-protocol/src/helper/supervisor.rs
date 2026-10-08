//! App side of a helper process: [`HelperAdapter`] and its supervisor task.
//!
//! One supervisor owns the helper of one protocol (ADR 0013). It is the only
//! code that starts and stops that helper, and it holds at most one process:
//! a new start waits until the old process ended, also after a kill. Two
//! processes on one session store break it. `handle` only sends a message
//! to the supervisor, so no caller ever waits on the pipe.
//!
//! Lifetime of the helper:
//!
//! - It starts when the user accepts the protocol's gate. Before that, the
//!   supervisor answers every command itself, like the feature-off stub.
//! - It stops when the app closes (`Shutdown`).
//! - If it ends by itself, the supervisor starts it again after a wait
//!   (ADR 0012 "Failure") and sends the gate and the pairing again, so the
//!   account links again from its session store. After the last try the
//!   account row shows "Helper stopped." and Restart.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use thinwire_ipc::{
    AppLine, FrameError, FrameReader, HelperLine, HelperRefusal, PROTOCOL_VERSION, WireCommand,
    WireProtocol, encode,
};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, WeakUnboundedSender};
use tokio::sync::oneshot;
use tokio::time::Instant;

use super::convert::{command_to_wire, event_from_wire, wire_protocol};
use super::launch::{HelperInput, HelperLauncher, HelperOutput, HelperProcess};
use crate::adapter::{
    APP_CLOSE_LIMIT, AccountState, AdapterCommand, AdapterError, AdapterEvent, AdapterStatus,
    EventTx, HelperFault, HelperState, ProtocolAdapter, ProtocolCapabilities, ProtocolId,
    emit_account, emit_chat_list_loaded, emit_command_failed, emit_helper, emit_history_loaded,
    emit_notice, emit_older_history_loaded, emit_send_rejected, emit_status, emit_stopped,
};
use crate::whatsapp::WhatsAppPhoneVault;

// region: timing

/// Longest wait of a helper for its adapter to close its sessions. An
/// adapter keeps its own shutdown bound below this wait: the WhatsApp
/// client takes 4 s at most (1 s for sends in flight, 3 s for the link).
pub const ADAPTER_STOP_WAIT: Duration = APP_CLOSE_LIMIT.saturating_sub(Duration::from_millis(500));

/// Longest wait of the app for `Stopped` from a helper. After the helper's
/// own wait, so a helper that gives up ends by itself first. Before the
/// app's close limit, so `Stopped` still comes in time.
pub(crate) const HELPER_STOP_WAIT: Duration =
    APP_CLOSE_LIMIT.saturating_sub(Duration::from_millis(250));

const _: () = assert!(
    ADAPTER_STOP_WAIT.as_millis() < HELPER_STOP_WAIT.as_millis()
        && HELPER_STOP_WAIT.as_millis() < APP_CLOSE_LIMIT.as_millis()
);

/// Waits and limits of the supervisor. Tests use short ones.
#[derive(Debug, Clone, Copy)]
pub struct HelperTiming {
    /// Longest wait for `Hello` after a start.
    pub hello_wait: Duration,
    /// A request with no `Ack` after this time makes the row busy.
    pub busy_after: Duration,
    /// Wait before the first restart. Each next wait is twice as long.
    pub restart_base: Duration,
    /// Longest wait before a restart.
    pub restart_max: Duration,
    /// Failures in a row before the supervisor stops trying.
    pub max_failures: u32,
    /// A helper that ran this long was not a failed start: the count of
    /// failures starts again.
    pub stable_run: Duration,
    /// Longest wait for `Stopped` after `Shutdown`.
    pub stop_wait: Duration,
    /// Longest wait for a killed helper process to end, before a new one
    /// starts.
    pub kill_wait: Duration,
}

impl HelperTiming {
    /// The waits of the app (ADR 0012 "Failure").
    pub const APP: Self = Self {
        hello_wait: Duration::from_secs(10),
        busy_after: Duration::from_millis(1500),
        restart_base: Duration::from_secs(1),
        restart_max: Duration::from_secs(60),
        max_failures: 5,
        stable_run: Duration::from_secs(60),
        stop_wait: HELPER_STOP_WAIT,
        kill_wait: Duration::from_secs(5),
    };

    /// The wait before restart number `failures` (1 for the first one).
    fn backoff(&self, failures: u32) -> Duration {
        let doubled = failures.saturating_sub(1).min(16);
        self.restart_base
            .saturating_mul(1 << doubled)
            .min(self.restart_max)
    }
}

impl Default for HelperTiming {
    fn default() -> Self {
        Self::APP
    }
}

// endregion: timing

// region: adapter

/// What the app knows about one helper protocol.
#[derive(Debug, Clone, Copy)]
pub struct HelperSpec {
    pub protocol: ProtocolId,
    /// The capabilities of the client in the helper. The app shows them
    /// also while no helper runs.
    pub capabilities: ProtocolCapabilities,
}

/// MIT adapter for a protocol that runs in a helper process. It implements
/// [`ProtocolAdapter`], so the core and the frontends see no difference.
pub struct HelperAdapter {
    spec: HelperSpec,
    launcher: Arc<dyn HelperLauncher>,
    phone: Option<Arc<WhatsAppPhoneVault>>,
    timing: HelperTiming,
    supervisor: Option<UnboundedSender<Msg>>,
}

impl HelperAdapter {
    /// An adapter that starts its helper through `launcher`. `phone` is the
    /// vault of the pairing screen, for a protocol with a pair code.
    #[must_use]
    pub fn new(
        spec: HelperSpec,
        launcher: Arc<dyn HelperLauncher>,
        phone: Option<Arc<WhatsAppPhoneVault>>,
    ) -> Self {
        Self {
            spec,
            launcher,
            phone,
            timing: HelperTiming::APP,
            supervisor: None,
        }
    }

    /// Use other waits. For tests.
    #[must_use]
    pub const fn with_timing(mut self, timing: HelperTiming) -> Self {
        self.timing = timing;
        self
    }

    fn send(&self, msg: Msg) {
        if let Some(supervisor) = &self.supervisor {
            let _ = supervisor.send(msg);
        }
    }
}

impl ProtocolAdapter for HelperAdapter {
    fn id(&self) -> ProtocolId {
        self.spec.protocol
    }

    fn capabilities(&self) -> ProtocolCapabilities {
        self.spec.capabilities
    }

    /// No helper starts here. The supervisor only checks that the program
    /// exists.
    fn start(&mut self, events: EventTx) {
        let protocol = self.spec.protocol;
        emit_status(
            &events,
            protocol,
            AdapterStatus::Stubbed,
            self.spec.capabilities.detail,
        );
        let Some(wire) = wire_protocol(protocol) else {
            tracing::error!(%protocol, "this protocol does not run in a helper");
            return;
        };
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut supervisor = Supervisor {
            protocol,
            wire,
            launcher: Arc::clone(&self.launcher),
            phone: self.phone.clone(),
            timing: self.timing,
            events,
            inbox: tx.downgrade(),
            link: None,
            runs: 0,
            reported: None,
            failures: 0,
            alive: HashSet::new(),
            start_waits: false,
            old_exit_deadline: None,
            restart_at: None,
            stop_deadline: None,
            stopping: false,
            replay: Replay::default(),
            wake_link: None,
            sends: HashMap::new(),
            account: AccountState::Unlinked,
            next_id: 1,
        };
        if !supervisor.launcher.installed() {
            supervisor.report(HelperState::Missing);
        }
        self.supervisor = Some(tx);
        tokio::spawn(supervisor.run(rx));
    }

    /// Never waits: the command goes to the supervisor's queue.
    fn handle(&mut self, command: AdapterCommand, _events: &EventTx) -> Result<(), AdapterError> {
        let protocol = self.spec.protocol;
        if matches!(command, AdapterCommand::RestartHelper { protocol: named } if named == protocol)
        {
            self.send(Msg::Restart);
            return Ok(());
        }
        // The phone is read later, on the supervisor. This is only the
        // check that a helper takes the command.
        if command_to_wire(protocol, &command, || None).is_none() {
            return Err(AdapterError::Unavailable {
                protocol,
                reason: "command is not handled by the helper adapter",
            });
        }
        self.send(Msg::Command(command));
        Ok(())
    }

    fn shutdown(&mut self, events: &EventTx) {
        if self.supervisor.is_some() {
            self.send(Msg::Shutdown);
        } else {
            emit_stopped(events, self.spec.protocol);
        }
    }

    fn view_chat(&mut self, conversation_id: Option<&str>, _events: &EventTx) {
        self.send(Msg::Command(AdapterCommand::ViewChat {
            protocol: self.spec.protocol,
            conversation_id: conversation_id.map(str::to_owned),
        }));
    }
}

// endregion: adapter

// region: supervisor

enum Msg {
    Command(AdapterCommand),
    /// The user clicked Restart on the account row.
    Restart,
    Shutdown,
    /// A line from the helper process number `run`.
    Line {
        run: u64,
        line: HelperLine,
    },
    /// The output of the helper process number `run` ended or broke. The
    /// process can still run.
    Ended {
        run: u64,
        why: Ended,
    },
    /// The helper process number `run` ended. The OS dropped its lock on
    /// the session store.
    Exited {
        run: u64,
    },
}

enum Ended {
    /// The process closed its output.
    Closed,
    /// A line that is not in the protocol.
    BadLine(FrameError),
}

impl std::fmt::Display for Ended {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed => f.write_str("the process closed its output"),
            Self::BadLine(error) => write!(f, "bad line: {error}"),
        }
    }
}

/// What the supervisor sends again after a restart, so the account links
/// again. The helper keeps the session store, so no new pairing is necessary
/// for a linked account. The phone of a pair-code pairing is not here: it
/// crosses the pipe one time, with the pairing start of the user (ADR 0012
/// "Secrets on the wire"). A pairing that a restart sends again has no
/// phone, so the helper asks for a QR code.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Replay {
    /// The user accepted the gate.
    acknowledged: bool,
    /// The pairing that the user started.
    link: Option<u64>,
}

impl Replay {
    const fn is_empty(self) -> bool {
        !self.acknowledged && self.link.is_none()
    }

    fn note(&mut self, command: &WireCommand) {
        match command {
            WireCommand::AcknowledgeGate => self.acknowledged = true,
            WireCommand::BeginLink { generation, .. } => self.link = Some(*generation),
            WireCommand::CancelLink | WireCommand::Disconnect => *self = Self::default(),
            _ => {}
        }
    }
}

/// The connection to a helper that runs. Dropping it closes the helper's
/// stdin and kills the process.
struct Link {
    run: u64,
    started: Instant,
    /// Encoded lines for the helper's stdin. A writer task owns the pipe.
    lines: UnboundedSender<String>,
    _kill: oneshot::Sender<()>,
    /// `Hello` came, and its version is the version of this app.
    greeted: bool,
    hello_deadline: Instant,
    /// Requests that wait for `Hello`.
    held: Vec<WireCommand>,
    /// Requests with no `Ack` yet, oldest first, with their send time.
    unacked: VecDeque<(u64, Instant)>,
}

struct Supervisor {
    protocol: ProtocolId,
    wire: WireProtocol,
    launcher: Arc<dyn HelperLauncher>,
    phone: Option<Arc<WhatsAppPhoneVault>>,
    timing: HelperTiming,
    events: EventTx,
    /// The supervisor's own queue, for the tasks of a helper process. Weak:
    /// when the adapter is dropped, the queue closes and the helper ends.
    inbox: WeakUnboundedSender<Msg>,
    link: Option<Link>,
    /// Number of helper processes started. It names the current one.
    runs: u64,
    /// Helper processes that did not end yet, by number. Dropping a link
    /// only sends the kill. A new process starts when this is empty, so no
    /// two processes use the session store, and the new one finds no lock.
    alive: HashSet<u64>,
    /// A start waits for the old process to end.
    start_waits: bool,
    /// When the wait for the old process ends without a start.
    old_exit_deadline: Option<Instant>,
    /// The last helper state sent to the shell.
    reported: Option<HelperState>,
    /// Failed runs in a row.
    failures: u32,
    restart_at: Option<Instant>,
    stop_deadline: Option<Instant>,
    /// The app closes. No command and no restart after this.
    stopping: bool,
    replay: Replay,
    /// A pairing start of the user that no helper got yet: its generation
    /// and the command, with its phone. It came while no helper ran, or the
    /// helper ended before `Hello`. It goes out one time, after the replay
    /// of the next start.
    wake_link: Option<(u64, WireCommand)>,
    /// Sends and retries with no answer yet: request id and chat. Each one
    /// gets `SendRejected` if the helper ends (ADR 0010 rule 4).
    sends: HashMap<u64, String>,
    /// The last link state that the helper reported.
    account: AccountState,
    next_id: u64,
}

impl Supervisor {
    async fn run(mut self, mut inbox: UnboundedReceiver<Msg>) {
        loop {
            let deadline = self.next_deadline();
            tokio::select! {
                msg = inbox.recv() => match msg {
                    Some(msg) => self.on_msg(msg),
                    // The adapter is gone. Dropping the link ends the helper.
                    None => return,
                },
                () = sleep_until(deadline) => self.on_timer(Instant::now()),
            }
        }
    }

    fn next_deadline(&self) -> Option<Instant> {
        let link = self.link.as_ref();
        let hello = link
            .filter(|link| !link.greeted)
            .map(|link| link.hello_deadline);
        let busy = link
            .filter(|link| link.greeted && self.reported == Some(HelperState::Running))
            .and_then(|link| link.unacked.front())
            .map(|(_, sent)| *sent + self.timing.busy_after);
        [
            self.restart_at,
            self.stop_deadline,
            self.old_exit_deadline,
            hello,
            busy,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    fn on_timer(&mut self, now: Instant) {
        if self.stop_deadline.is_some_and(|at| at <= now) {
            // The helper did not confirm in time. Ending the process ends
            // every client in it, so `Stopped` is true.
            self.finish_stop();
            return;
        }
        if self.old_exit_deadline.is_some_and(|at| at <= now) {
            // The old process got its kill and still runs. A second process
            // on its session store is not safe: stop, and let the user try.
            tracing::warn!(protocol = %self.protocol, "the old helper process did not end");
            self.start_waits = false;
            self.old_exit_deadline = None;
            self.stop_for(HelperFault::StillRunning);
            return;
        }
        if self.restart_at.is_some_and(|at| at <= now) {
            self.restart_at = None;
            self.launch();
            return;
        }
        let Some(link) = &self.link else {
            return;
        };
        if !link.greeted && link.hello_deadline <= now {
            tracing::warn!(protocol = %self.protocol, "helper sent no Hello in time");
            self.helper_ended();
            return;
        }
        let slow = link
            .unacked
            .front()
            .is_some_and(|(_, sent)| *sent + self.timing.busy_after <= now);
        if link.greeted && slow {
            self.report(HelperState::Busy);
        }
    }

    fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Command(command) => self.on_command(&command),
            Msg::Restart => self.on_restart(),
            Msg::Shutdown => self.on_shutdown(),
            Msg::Line { run, line } if self.is_current(run) => self.on_line(line),
            Msg::Ended { run, why } if self.is_current(run) => {
                tracing::info!(protocol = %self.protocol, %why, "helper output ended");
                self.on_helper_gone();
            }
            Msg::Exited { run } => {
                self.alive.remove(&run);
                if self.is_current(run) {
                    tracing::info!(protocol = %self.protocol, "helper process ended");
                    self.on_helper_gone();
                }
                // A start that waited for this process can go now.
                if self.start_waits && self.alive.is_empty() {
                    self.launch();
                }
            }
            // From a process that the supervisor already dropped.
            Msg::Line { .. } | Msg::Ended { .. } => {}
        }
    }

    /// The current helper ended, or its output did.
    fn on_helper_gone(&mut self) {
        if self.stopping {
            self.finish_stop();
        } else {
            self.helper_ended();
        }
    }

    fn is_current(&self, run: u64) -> bool {
        self.link.as_ref().is_some_and(|link| link.run == run)
    }

    // region: commands

    fn on_command(&mut self, command: &AdapterCommand) {
        if self.stopping {
            return;
        }
        let phone = self.phone.clone();
        let Some(wire) = command_to_wire(self.protocol, command, || {
            phone.and_then(|vault| vault.phone())
        }) else {
            return;
        };
        if self.link.is_some() {
            self.replay.note(&wire);
            self.request(wire);
            return;
        }
        // The gate and a pairing start wake the helper.
        match wire {
            // `launch` sends the gate from the replay.
            WireCommand::AcknowledgeGate => {
                self.replay.note(&wire);
                self.failures = 0;
                self.launch();
            }
            // This pairing takes the place of the one in the replay.
            // `launch` sends it after the replay, with its phone.
            WireCommand::BeginLink { generation, .. } => {
                self.replay.link = None;
                self.wake_link = Some((generation, wire));
                self.failures = 0;
                self.launch();
            }
            other => {
                self.replay.note(&other);
                self.answer_without_helper(&other);
            }
        }
    }

    /// Send one request to the helper, or keep it until `Hello`.
    fn request(&mut self, command: WireCommand) {
        if let WireCommand::SendText {
            conversation_id,
            request,
            ..
        }
        | WireCommand::ResendMessage {
            conversation_id,
            request,
            ..
        } = &command
        {
            self.sends.insert(*request, conversation_id.clone());
        }
        let Some(link) = &mut self.link else {
            return;
        };
        if !link.greeted {
            link.held.push(command);
            return;
        }
        let id = self.next_id;
        self.next_id += 1;
        let line = AppLine::Request {
            id,
            protocol: self.wire,
            command,
        };
        match encode(&line) {
            Ok(text) => {
                link.unacked.push_back((id, Instant::now()));
                let _ = link.lines.send(text);
            }
            // The line is over the size limit. The pipe stays open, and the
            // command ends here.
            Err(error) => {
                tracing::warn!(protocol = %self.protocol, %error, "helper request not sent");
                let AppLine::Request { command, .. } = line else {
                    return;
                };
                self.answer_without_helper(&command);
            }
        }
    }

    /// End a command that no helper takes (ADR 0010 rules 4, 5 and 9): a
    /// send with `SendRejected`, a load with `CommandFailed` and its end
    /// event. The feature-off stub gives the same answers.
    fn answer_without_helper(&mut self, command: &WireCommand) {
        let protocol = self.protocol;
        let events = &self.events.clone();
        match command {
            WireCommand::Connect => self.send_idle_status(),
            WireCommand::LoadChats => {
                emit_command_failed(events, protocol, None, self.not_connected());
                emit_chat_list_loaded(events, protocol);
            }
            WireCommand::OpenChat { conversation_id } => {
                emit_command_failed(
                    events,
                    protocol,
                    Some(conversation_id.clone()),
                    self.not_connected(),
                );
                emit_history_loaded(events, protocol, conversation_id.clone());
            }
            WireCommand::LoadOlderMessages {
                conversation_id,
                before_message_id,
            } => emit_older_history_loaded(
                events,
                protocol,
                conversation_id.clone(),
                before_message_id.clone(),
                false,
                None,
            ),
            WireCommand::SendText {
                conversation_id,
                request,
                ..
            }
            | WireCommand::ResendMessage {
                conversation_id,
                request,
                ..
            } => {
                self.sends.remove(request);
                emit_send_rejected(events, protocol, conversation_id.clone(), *request);
            }
            WireCommand::CancelLink | WireCommand::Disconnect => {
                // The user gave up the link: no restart, and no account.
                self.restart_at = None;
                self.wake_link = None;
                if self.reported == Some(HelperState::Restarting) {
                    self.report(HelperState::Idle);
                }
                self.account = AccountState::Unlinked;
                emit_account(events, protocol, AccountState::Unlinked);
                self.send_idle_status();
            }
            WireCommand::ViewChat { .. } => {}
            // A start that failed: `launch` reported it.
            WireCommand::AcknowledgeGate | WireCommand::BeginLink { .. } => {}
        }
    }

    fn send_idle_status(&self) {
        let (status, detail) = match self.reported {
            Some(HelperState::Missing) => (AdapterStatus::Error, self.missing()),
            Some(HelperState::Stopped(_)) => (AdapterStatus::Error, self.stopped()),
            _ => (
                AdapterStatus::Stubbed,
                format!("{} has no linked device.", self.protocol),
            ),
        };
        emit_status(&self.events, self.protocol, status, detail);
    }

    // endregion: commands

    // region: process

    /// Start the helper and queue the replay. Only this function starts a
    /// helper, and only when no process runs.
    fn launch(&mut self) {
        self.restart_at = None;
        if self.link.is_some() || self.stopping {
            return;
        }
        if !self.alive.is_empty() {
            // The old process got its kill, but the OS did not report its
            // end yet. It can still hold the lock of the session store.
            // `Exited` starts the new process.
            self.start_waits = true;
            self.old_exit_deadline
                .get_or_insert(Instant::now() + self.timing.kill_wait);
            return;
        }
        self.start_waits = false;
        self.old_exit_deadline = None;
        if !self.launcher.installed() {
            self.wake_link = None;
            self.report(HelperState::Missing);
            emit_status(
                &self.events,
                self.protocol,
                AdapterStatus::Error,
                self.missing(),
            );
            return;
        }
        let process = match self.launcher.launch() {
            Ok(process) => process,
            Err(error) => {
                tracing::warn!(protocol = %self.protocol, kind = %error.kind(), "helper did not start");
                self.stop_for(HelperFault::StartFailed);
                return;
            }
        };
        self.runs += 1;
        let run = self.runs;
        self.alive.insert(run);
        let HelperProcess {
            stdin,
            stdout,
            stderr,
            exited,
            kill,
            pid,
        } = process;
        tracing::info!(protocol = %self.protocol, run, ?pid, "helper started");
        let (lines, line_rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(write_lines(stdin, line_rx));
        tokio::spawn(read_lines(stdout, run, self.inbox.clone()));
        if let Some(stderr) = stderr {
            tokio::spawn(log_stderr(stderr, self.protocol));
        }
        let inbox = self.inbox.clone();
        tokio::spawn(async move {
            let _ = exited.await;
            notify(&inbox, Msg::Exited { run });
        });
        let now = Instant::now();
        self.link = Some(Link {
            run,
            started: now,
            lines,
            _kill: kill,
            greeted: false,
            hello_deadline: now + self.timing.hello_wait,
            held: self.start_commands(),
            unacked: VecDeque::new(),
        });
        self.report(HelperState::Busy);
    }

    /// The first commands of a new helper: the replay, then the pairing
    /// start of the user that woke it, if one did.
    fn start_commands(&mut self) -> Vec<WireCommand> {
        let mut commands = Vec::new();
        if self.replay.acknowledged {
            commands.push(WireCommand::AcknowledgeGate);
        }
        if let Some(generation) = self.replay.link {
            // No phone: it crossed the pipe with the first start.
            commands.push(WireCommand::BeginLink {
                generation,
                phone: None,
            });
        }
        if let Some((generation, command)) = self.wake_link.take() {
            self.replay.link = Some(generation);
            commands.push(command);
        }
        commands
    }

    fn on_line(&mut self, line: HelperLine) {
        let greeted = self.link.as_ref().is_some_and(|link| link.greeted);
        match line {
            HelperLine::Hello {
                protocol_version,
                helper_version,
                protocols,
            } if !greeted => {
                if protocol_version != PROTOCOL_VERSION || !protocols.contains(&self.wire) {
                    tracing::warn!(
                        protocol = %self.protocol,
                        protocol_version,
                        %helper_version,
                        "helper speaks another wire protocol"
                    );
                    self.stop_for(HelperFault::VersionMismatch);
                    return;
                }
                tracing::info!(protocol = %self.protocol, %helper_version, "helper is up");
                let held = self.link.as_mut().map_or_else(Vec::new, |link| {
                    link.greeted = true;
                    std::mem::take(&mut link.held)
                });
                self.report(HelperState::Running);
                for command in held {
                    self.request(command);
                }
            }
            HelperLine::Refused { reason } if !greeted => {
                tracing::warn!(protocol = %self.protocol, ?reason, "helper refused to run");
                self.stop_for(match reason {
                    HelperRefusal::SessionInUse => HelperFault::SessionInUse,
                    HelperRefusal::NoDataDir => HelperFault::StartFailed,
                });
            }
            HelperLine::Ack { id } if greeted => self.on_ack(id),
            HelperLine::Event { protocol, event } if greeted && protocol == self.wire => {
                self.on_event(event);
            }
            HelperLine::Stopped { protocol } if greeted && protocol == self.wire => {
                if self.stopping {
                    self.finish_stop();
                }
            }
            // A line at the wrong time, or for another protocol: the helper
            // is broken.
            other => {
                tracing::warn!(protocol = %self.protocol, line = ?other, "helper sent a line out of place");
                self.helper_ended();
            }
        }
    }

    fn on_ack(&mut self, id: u64) {
        let Some(link) = &mut self.link else {
            return;
        };
        while link.unacked.front().is_some_and(|(sent, _)| *sent <= id) {
            link.unacked.pop_front();
        }
        let now = Instant::now();
        let slow = link
            .unacked
            .front()
            .is_some_and(|(_, sent)| *sent + self.timing.busy_after <= now);
        if !slow && self.reported == Some(HelperState::Busy) {
            self.report(HelperState::Running);
        }
    }

    fn on_event(&mut self, event: thinwire_ipc::WireEvent) {
        let Some(event) = event_from_wire(self.protocol, event) else {
            tracing::debug!(protocol = %self.protocol, "helper event dropped: not for this protocol");
            return;
        };
        match &event {
            AdapterEvent::Account { state, .. } => self.account = *state,
            AdapterEvent::SendAccepted { request, .. }
            | AdapterEvent::SendRejected { request, .. } => {
                self.sends.remove(request);
            }
            _ => {}
        }
        let _ = self.events.send(event);
    }

    /// The helper ended by itself, or it broke the protocol (ADR 0012
    /// "Failure").
    fn helper_ended(&mut self) {
        let Some(link) = self.link.take() else {
            return;
        };
        let stable = link.started.elapsed() >= self.timing.stable_run;
        self.keep_unsent_pairing(&link);
        // Dropping the link ends the process, if it still runs.
        drop(link);
        self.reject_open_sends();
        if self.replay.is_empty() && self.wake_link.is_none() {
            // The user has no link to keep: no restart.
            self.failures = 0;
            self.report(HelperState::Idle);
            return;
        }
        self.failures = if stable { 1 } else { self.failures + 1 };
        if self.failures >= self.timing.max_failures {
            self.stop_for(HelperFault::Crashed);
            return;
        }
        // A session that was up reconnects. The shell keeps its rows, ends
        // its loads, and asks again after `Linked` (#163).
        if self.account != AccountState::Unlinked {
            self.account = AccountState::Linking;
            emit_account(&self.events, self.protocol, AccountState::Linking);
        }
        self.report(HelperState::Restarting);
        emit_notice(
            &self.events,
            self.protocol,
            format!(
                "The {} helper stopped. thinwire starts it again.",
                self.protocol
            ),
        );
        self.restart_at = Some(Instant::now() + self.timing.backoff(self.failures));
    }

    /// A pairing start that waited for `Hello` never crossed the pipe. Keep
    /// it, with its phone, for the next helper. Else the replay sends it
    /// with no phone, and a pair-code pairing of the user becomes a QR
    /// pairing though the helper never got the number.
    fn keep_unsent_pairing(&mut self, link: &Link) {
        if link.greeted {
            return;
        }
        let unsent = link.held.iter().rev().find_map(|command| match command {
            WireCommand::BeginLink { generation, .. } => Some((*generation, command.clone())),
            _ => None,
        });
        if let Some(unsent) = unsent {
            self.replay.link = None;
            self.wake_link = Some(unsent);
        }
    }

    /// Stop for good, until the user clicks Restart or passes the gate
    /// again.
    fn stop_for(&mut self, fault: HelperFault) {
        self.link = None;
        self.restart_at = None;
        self.wake_link = None;
        self.reject_open_sends();
        if self.account != AccountState::Unlinked {
            self.account = AccountState::Unlinked;
            emit_account(&self.events, self.protocol, AccountState::Unlinked);
        }
        self.report(HelperState::Stopped(fault));
        emit_status(
            &self.events,
            self.protocol,
            AdapterStatus::Error,
            self.stopped(),
        );
    }

    fn reject_open_sends(&mut self) {
        for (request, conversation_id) in self.sends.drain() {
            emit_send_rejected(&self.events, self.protocol, conversation_id, request);
        }
    }

    fn on_restart(&mut self) {
        if self.stopping || self.link.is_some() {
            return;
        }
        self.failures = 0;
        self.launch();
    }

    fn on_shutdown(&mut self) {
        if self.stopping {
            return;
        }
        self.stopping = true;
        self.restart_at = None;
        self.wake_link = None;
        self.start_waits = false;
        self.old_exit_deadline = None;
        let Some(link) = &self.link else {
            self.finish_stop();
            return;
        };
        if let Ok(line) = encode(&AppLine::Shutdown) {
            let _ = link.lines.send(line);
        }
        self.stop_deadline = Some(Instant::now() + self.timing.stop_wait);
    }

    /// The helper process is gone, or it goes now. No client runs.
    fn finish_stop(&mut self) {
        self.link = None;
        self.stop_deadline = None;
        self.sends.clear();
        emit_stopped(&self.events, self.protocol);
    }

    // endregion: process

    fn report(&mut self, state: HelperState) {
        if self.reported != Some(state) {
            self.reported = Some(state);
            emit_helper(&self.events, self.protocol, state);
        }
    }

    fn not_connected(&self) -> String {
        format!("{} is not connected. Link a device first.", self.protocol)
    }

    fn missing(&self) -> String {
        format!("{} helper missing. Reinstall thinwire.", self.protocol)
    }

    fn stopped(&self) -> String {
        format!(
            "The {} helper stopped. Click Restart on the {} account.",
            self.protocol, self.protocol
        )
    }
}

// endregion: supervisor

// region: tasks

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

fn notify(inbox: &WeakUnboundedSender<Msg>, msg: Msg) {
    if let Some(inbox) = inbox.upgrade() {
        let _ = inbox.send(msg);
    }
}

/// Owns the helper's stdin. It ends when the link is dropped, and the pipe
/// then closes.
async fn write_lines(mut stdin: HelperInput, mut lines: UnboundedReceiver<String>) {
    while let Some(line) = lines.recv().await {
        if stdin.write_all(line.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
            return;
        }
    }
}

/// Reads the helper's stdout until it ends or breaks.
async fn read_lines(stdout: HelperOutput, run: u64, inbox: WeakUnboundedSender<Msg>) {
    let mut frames = FrameReader::new(BufReader::new(stdout));
    let why = loop {
        match frames.next::<HelperLine>().await {
            Ok(Some(line)) => notify(&inbox, Msg::Line { run, line }),
            Ok(None) => break Ended::Closed,
            Err(error) => break Ended::BadLine(error),
        }
    };
    notify(&inbox, Msg::Ended { run, why });
}

/// Writes the helper's log lines at level `debug`. The helper never logs
/// message text, phone numbers, QR data, or pair codes (ADR 0012 section 4).
async fn log_stderr(stderr: HelperOutput, protocol: ProtocolId) {
    let mut frames = FrameReader::new(BufReader::new(stderr));
    loop {
        match frames.next_line().await {
            Ok(Some(line)) => tracing::debug!(%protocol, "helper: {line}"),
            Ok(None) => return,
            Err(_) => break,
        }
    }
    // A line that is too long or not UTF-8: stop logging, but keep reading,
    // so the helper never waits on a full pipe.
    let mut rest = frames.into_inner();
    let _ = tokio::io::copy(&mut rest, &mut tokio::io::sink()).await;
}

// endregion: tasks
