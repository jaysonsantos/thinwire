// SPDX-License-Identifier: AGPL-3.0-only
//! The real `thinwire-whatsapp-helper` process behind the MIT helper adapter
//! (#246, ADR 0013).
//!
//! Each test starts the helper program as a child process, as the app does.
//! No test starts a pairing: a pairing opens a network session. Each test
//! has its own session folder (`--session-dir`), so no test touches the
//! WhatsApp session of the user.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use thinwire_ipc::{HelperLine, PROTOCOL_VERSION, WireProtocol};
use thinwire_protocol::helper::{
    HelperLauncher, HelperProcess, HelperSpec, HelperTiming, ProcessLauncher,
};
use thinwire_protocol::{
    AdapterCommand, AdapterEvent, EventTx, HelperAdapter, HelperFault, HelperState,
    ProtocolAdapter, ProtocolId, WHATSAPP_HELPER_PROGRAM,
};
use thinwire_whatsapp::WhatsAppAdapter;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

/// The helper program that cargo built for this test run.
const HELPER: &str = env!("CARGO_BIN_EXE_thinwire-whatsapp-helper");
const PROTOCOL: ProtocolId = ProtocolId::WhatsApp;
const CHAT: &str = "whatsapp:111@s.whatsapp.net";
/// Longest wait for one event, or for the helper process to end.
const WAIT: Duration = Duration::from_secs(20);
/// Exit code of the helper for a line that is not in the protocol.
const EXIT_BAD_LINE: i32 = 4;

/// Short restart waits. Two failures in a row stop the helper for good.
const TIMING: HelperTiming = HelperTiming {
    hello_wait: Duration::from_secs(15),
    busy_after: Duration::from_secs(5),
    restart_base: Duration::from_millis(50),
    restart_max: Duration::from_millis(200),
    max_failures: 2,
    stable_run: Duration::from_secs(600),
    stop_wait: Duration::from_secs(4),
};

/// A new, empty session folder for one test.
fn session_dir(test: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "thinwire-whatsapp-helper-test-{}-{test}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("session folder");
    dir
}

/// Starts the real helper and keeps the process ids, so a test can kill one.
struct Recording {
    inner: ProcessLauncher,
    pids: Arc<Mutex<Vec<u32>>>,
}

impl HelperLauncher for Recording {
    fn installed(&self) -> bool {
        self.inner.installed()
    }

    fn launch(&self) -> std::io::Result<HelperProcess> {
        let process = self.inner.launch()?;
        if let Some(pid) = process.pid {
            self.pids.lock().expect("pids").push(pid);
        }
        Ok(process)
    }
}

/// The MIT helper adapter over the real helper process.
struct App {
    adapter: HelperAdapter,
    pids: Arc<Mutex<Vec<u32>>>,
    tx: EventTx,
    rx: UnboundedReceiver<AdapterEvent>,
    seen: Vec<AdapterEvent>,
    dir: PathBuf,
}

impl App {
    fn start(test: &str) -> Self {
        Self::start_in(session_dir(test))
    }

    fn start_in(dir: PathBuf) -> Self {
        let pids = Arc::new(Mutex::new(Vec::new()));
        let launcher = Recording {
            inner: ProcessLauncher::new(WHATSAPP_HELPER_PROGRAM, Some(PathBuf::from(HELPER)))
                .with_args(["--session-dir".to_owned(), dir.display().to_string()]),
            pids: Arc::clone(&pids),
        };
        let mut adapter = HelperAdapter::new(
            HelperSpec {
                protocol: PROTOCOL,
                capabilities: WhatsAppAdapter::capabilities(),
            },
            Arc::new(launcher),
            None,
        )
        .with_timing(TIMING);
        let (tx, rx) = unbounded_channel();
        adapter.start(tx.clone());
        Self {
            adapter,
            pids,
            tx,
            rx,
            seen: Vec::new(),
            dir,
        }
    }

    fn send(&mut self, command: AdapterCommand) {
        self.adapter
            .handle(command, &self.tx)
            .expect("the helper adapter takes the command");
    }

    async fn until(&mut self, what: &str, test: impl Fn(&AdapterEvent) -> bool) -> AdapterEvent {
        loop {
            let event = tokio::time::timeout(WAIT, self.rx.recv())
                .await
                .unwrap_or_else(|_| panic!("no {what} in {WAIT:?}. Seen: {:?}", self.seen))
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

    /// Accept the gate: the helper starts. No pairing, so no network.
    async fn accept_gate(&mut self) {
        self.send(AdapterCommand::WhatsAppAcknowledgeRisk);
        self.until_helper(HelperState::Running).await;
    }

    fn pids(&self) -> Vec<u32> {
        self.pids.lock().expect("pids").clone()
    }

    /// End the newest helper process from outside, like a crash.
    fn kill_helper(&self) {
        let pid = *self.pids().last().expect("a helper process");
        let killed = if cfg!(windows) {
            Command::new("taskkill")
                .args(["/F", "/PID", &pid.to_string()])
                .output()
        } else {
            Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .output()
        }
        .expect("the kill command ran");
        assert!(killed.status.success(), "the helper was not killed");
    }

    async fn shut_down(mut self) {
        self.adapter.shutdown(&self.tx);
        self.until("Stopped", |event| {
            *event == AdapterEvent::Stopped { protocol: PROTOCOL }
        })
        .await;
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The release script reads this text: it refuses a helper with no client.
#[test]
fn the_version_names_the_build() {
    let output = Command::new(HELPER)
        .arg("--version")
        .output()
        .expect("the helper ran");
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).expect("utf8");
    assert!(text.starts_with("thinwire-whatsapp-helper "), "{text}");
    let build = if cfg!(feature = "whatsapp-web") {
        "(whatsapp-web)"
    } else {
        "(no client)"
    };
    assert!(text.contains(build), "{text}");
}

/// The app's adapter starts the helper program at the gate, and commands
/// and events cross the pipe both ways.
#[tokio::test(flavor = "multi_thread")]
async fn the_app_talks_to_the_helper_process() {
    let mut app = App::start("round-trip");
    assert!(app.pids().is_empty(), "no process before the gate");
    app.accept_gate().await;
    assert_eq!(app.pids().len(), 1);

    app.send(AdapterCommand::LoadChats { protocol: PROTOCOL });
    let failed = app
        .until("CommandFailed", |event| {
            matches!(event, AdapterEvent::CommandFailed { .. })
        })
        .await;
    let AdapterEvent::CommandFailed { detail, .. } = failed else {
        unreachable!("matched above");
    };
    assert_eq!(
        detail, "WhatsApp is not connected. Pass the ban gate and pair a device first.",
        "the answer of the adapter in the helper"
    );
    app.until("ChatListLoaded", |event| {
        *event == AdapterEvent::ChatListLoaded { protocol: PROTOCOL }
    })
    .await;

    app.send(AdapterCommand::SendText {
        protocol: PROTOCOL,
        conversation_id: CHAT.into(),
        body: "hello".into(),
        request: 31,
    });
    app.until("SendRejected", |event| {
        *event
            == AdapterEvent::SendRejected {
                protocol: PROTOCOL,
                conversation_id: CHAT.into(),
                request: 31,
            }
    })
    .await;
    app.shut_down().await;
}

/// #246 "Helper crash test": kill the helper. It starts again. After the
/// last try the adapter reports `Stopped`, which the account row shows as
/// "Helper stopped." with Restart. Restart starts the helper again.
#[tokio::test(flavor = "multi_thread")]
async fn a_killed_helper_starts_again_and_stops_after_the_last_try() {
    let mut app = App::start("crash");
    app.accept_gate().await;

    app.kill_helper();
    app.until_helper(HelperState::Restarting).await;
    app.until_helper(HelperState::Running).await;
    assert_eq!(app.pids().len(), 2, "a new process after the kill");

    app.kill_helper();
    app.until_helper(HelperState::Stopped(HelperFault::Crashed))
        .await;
    tokio::time::sleep(TIMING.restart_max * 2).await;
    assert_eq!(app.pids().len(), 2, "no start without Restart");

    app.send(AdapterCommand::RestartHelper { protocol: PROTOCOL });
    app.until_helper(HelperState::Running).await;
    assert_eq!(app.pids().len(), 3);
    app.shut_down().await;
}

/// ADR 0013: two helpers never use one session store. The second process
/// finds the lock, refuses to run, and is not started again.
#[tokio::test(flavor = "multi_thread")]
async fn a_second_helper_on_the_same_session_is_refused() {
    let mut first = App::start("lock");
    first.accept_gate().await;

    let mut second = App::start_in(first.dir.clone());
    second.send(AdapterCommand::WhatsAppAcknowledgeRisk);
    second
        .until_helper(HelperState::Stopped(HelperFault::SessionInUse))
        .await;

    // The first helper ends: its lock ends, and the second app can start.
    let dir = first.dir.clone();
    first.adapter.shutdown(&first.tx);
    first
        .until("Stopped", |event| {
            *event == AdapterEvent::Stopped { protocol: PROTOCOL }
        })
        .await;
    // The first process can still hold the lock for a moment after
    // `Stopped`: try Restart until the helper runs.
    let deadline = Instant::now() + WAIT;
    loop {
        second.send(AdapterCommand::RestartHelper { protocol: PROTOCOL });
        let state = second
            .until("Running or Stopped", |event| {
                matches!(
                    event,
                    AdapterEvent::Helper {
                        state: HelperState::Running | HelperState::Stopped(_),
                        ..
                    }
                )
            })
            .await;
        if matches!(
            state,
            AdapterEvent::Helper {
                state: HelperState::Running,
                ..
            }
        ) {
            break;
        }
        assert!(Instant::now() < deadline, "the lock never ended");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    second.shut_down().await;
    let _ = std::fs::remove_dir_all(dir);
}

// region: the helper on its own

fn spawn_helper(dir: &std::path::Path) -> Child {
    Command::new(HELPER)
        .arg("--session-dir")
        .arg(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("the helper started")
}

/// Wait for the process to end. Kills it after [`WAIT`], so a failed test
/// leaves no process.
fn exit_code(child: &mut Child) -> Option<i32> {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(status) = child.try_wait().expect("wait") {
            return status.code();
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("the helper did not end in {WAIT:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn read_hello(child: &mut Child) -> BufReader<std::process::ChildStdout> {
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));
    let mut line = String::new();
    stdout.read_line(&mut line).expect("the first line");
    let hello: HelperLine = thinwire_ipc::decode(line.trim_end()).expect("a wire line");
    assert_eq!(
        hello,
        HelperLine::Hello {
            protocol_version: PROTOCOL_VERSION,
            helper_version: env!("CARGO_PKG_VERSION").into(),
            protocols: vec![WireProtocol::WhatsApp],
        },
        "stdout starts with Hello, and nothing else writes to it"
    );
    stdout
}

/// The app is gone (it closed the pipe, or it was killed): the helper ends
/// by itself. No orphan process keeps the session.
#[test]
fn the_helper_ends_when_the_app_closes_the_pipe() {
    let dir = session_dir("pipe-closed");
    let mut child = spawn_helper(&dir);
    let _stdout = read_hello(&mut child);
    drop(child.stdin.take());
    assert_eq!(exit_code(&mut child), Some(0));
    let _ = std::fs::remove_dir_all(dir);
}

/// ADR 0012 section 4: the helper closes on a line that is not in the
/// protocol.
#[test]
fn the_helper_closes_on_a_bad_line() {
    let dir = session_dir("bad-line");
    let mut child = spawn_helper(&dir);
    let _stdout = read_hello(&mut child);
    let mut stdin = child.stdin.take().expect("stdin");
    stdin
        .write_all(b"{\"type\":\"format_disk\"}\n")
        .expect("write");
    stdin.flush().expect("flush");
    assert_eq!(exit_code(&mut child), Some(EXIT_BAD_LINE));
    let _ = std::fs::remove_dir_all(dir);
}

// endregion: the helper on its own
