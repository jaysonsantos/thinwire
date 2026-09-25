//! Local-only Signal adapter (presage / libsignal, AGPL).
//!
//! The default build keeps a stub and does not open a network session.
//! Feature `signal-local` may link a secondary device only after the UI has
//! accepted the full-screen notice. Secrets and message text never travel on
//! commands and are not logged.

mod device;
mod group;
mod message;
mod path;
mod reconnect;
mod time;

#[cfg(feature = "signal-local")]
mod live;

#[cfg(feature = "signal-local")]
use std::sync::Arc;

use crate::adapter::{
    AdapterCommand, AdapterError, AdapterEvent, AdapterStatus, EventTx, ProtocolAdapter,
    ProtocolCapabilities, ProtocolId, RedactedPairingSecret, SupportClass, emit_conversation,
    emit_message, emit_status, emit_stopped,
};

#[cfg(not(feature = "signal-local"))]
use device::FeatureOff;
use device::SignalDevice;

#[cfg(not(feature = "signal-local"))]
const CAPABILITY_DETAIL: &str = "Local-only secondary device (presage). The signal-local feature is off in this build. Not in release builds.";

#[cfg(feature = "signal-local")]
const CAPABILITY_DETAIL: &str = "Local-only secondary device via presage. AGPL. Experimental. Not in release builds or OS zips.";

const CAPABILITIES: ProtocolCapabilities = ProtocolCapabilities {
    id: ProtocolId::Signal,
    support: SupportClass::Experimental,
    short_label: "Local only · AGPL · not in releases",
    detail: CAPABILITY_DETAIL,
    official_api: false,
    allows_user_account_automation: false,
    sends_text: true,
};

const NOTICE_REQUIRED: &str =
    "Signal linking is refused until the full-screen local-build notice is accepted";

pub(crate) const FEATURE_OFF: &str =
    "signal-local is off in this build. No Signal session is started.";

/// Join budget for the Signal thread. The app closes the window at 5 seconds.
#[cfg(feature = "signal-local")]
const SHUTDOWN_LIMIT: std::time::Duration = std::time::Duration::from_secs(4);

enum Engine {
    /// Test double and the feature-off stub. A `signal-local` library build
    /// uses `Live`; tests still construct this variant.
    #[cfg_attr(feature = "signal-local", allow(dead_code))]
    Sync(Box<dyn SignalDevice>),
    #[cfg(feature = "signal-local")]
    Live(Arc<live::Session>),
}

/// Local-only Signal adapter. Network linking exists only with `signal-local`
/// and only after [`AdapterCommand::SignalAcknowledgeNotice`].
pub struct SignalAdapter {
    notice_accepted: bool,
    engine: Engine,
    #[cfg(feature = "signal-local")]
    worker: Option<SignalWorker>,
}

impl SignalAdapter {
    #[must_use]
    pub fn new() -> Self {
        Self {
            notice_accepted: false,
            #[cfg(feature = "signal-local")]
            engine: Engine::Live(Arc::new(live::Session::new())),
            #[cfg(not(feature = "signal-local"))]
            engine: Engine::Sync(Box::new(FeatureOff)),
            #[cfg(feature = "signal-local")]
            worker: None,
        }
    }

    #[cfg(test)]
    fn with_device(device: impl SignalDevice + 'static) -> Self {
        Self {
            notice_accepted: false,
            engine: Engine::Sync(Box::new(device)),
            #[cfg(feature = "signal-local")]
            worker: None,
        }
    }

    #[must_use]
    pub const fn capabilities() -> ProtocolCapabilities {
        CAPABILITIES
    }

    fn acknowledge(&mut self, events: &EventTx) -> Result<(), AdapterError> {
        self.notice_accepted = true;
        emit_status(
            events,
            ProtocolId::Signal,
            AdapterStatus::Stubbed,
            "Signal local-build notice accepted. No session has started.",
        );
        Ok(())
    }

    fn begin_link(&mut self, events: &EventTx) -> Result<(), AdapterError> {
        if !self.notice_accepted {
            return Err(AdapterError::Refused {
                protocol: ProtocolId::Signal,
                reason: NOTICE_REQUIRED,
            });
        }
        #[cfg(feature = "signal-local")]
        if matches!(self.engine, Engine::Live(_)) {
            self.worker = self.spawn_worker(events);
            emit_status(
                events,
                ProtocolId::Signal,
                AdapterStatus::Connecting,
                "Signal linking was queued on the worker. This build is local only.",
            );
            return Ok(());
        }
        match &mut self.engine {
            Engine::Sync(device) => match device.link() {
                Ok(Some(url)) => {
                    let _ = events.send(AdapterEvent::SignalQr {
                        code: RedactedPairingSecret::new(url),
                        generation: 1,
                    });
                    emit_status(
                        events,
                        ProtocolId::Signal,
                        AdapterStatus::Ready,
                        "Signal device linked in this process.",
                    );
                    Ok(())
                }
                Ok(None) => {
                    emit_status(
                        events,
                        ProtocolId::Signal,
                        AdapterStatus::Ready,
                        "Signal device was already linked.",
                    );
                    Ok(())
                }
                Err(FEATURE_OFF) => Err(AdapterError::Unavailable {
                    protocol: ProtocolId::Signal,
                    reason: FEATURE_OFF,
                }),
                Err(reason) => Err(AdapterError::Unavailable {
                    protocol: ProtocolId::Signal,
                    reason,
                }),
            },
            #[cfg(feature = "signal-local")]
            Engine::Live(_) => Ok(()),
        }
    }

    #[cfg(feature = "signal-local")]
    fn spawn_worker(&self, events: &EventTx) -> Option<SignalWorker> {
        let Engine::Live(session) = &self.engine else {
            return None;
        };
        let token = session.next_generation();
        session.mark_active();
        let session = Arc::clone(session);
        let task_events = events.clone();
        // Presage's store is not `Send`. It runs on its own current-thread
        // runtime, off the UI thread.
        let (abort_tx, abort_rx) = std::sync::mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("thinwire-signal".into())
            .spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    emit_status(
                        &task_events,
                        ProtocolId::Signal,
                        AdapterStatus::Error,
                        "Signal worker runtime could not start. No session was opened.",
                    );
                    return;
                };
                let local = tokio::task::LocalSet::new();
                local.block_on(&runtime, async move {
                    let task = tokio::task::spawn_local(live::run(session, token, task_events));
                    let _ = abort_tx.send(task.abort_handle());
                    let _ = task.await;
                });
            })
            .ok()?;
        let abort = abort_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .ok()?;
        Some(SignalWorker { thread, abort })
    }

    fn cancel_link(&mut self, events: &EventTx) -> Result<(), AdapterError> {
        self.notice_accepted = false;
        #[cfg(feature = "signal-local")]
        if let Engine::Live(session) = &self.engine {
            session.next_generation();
            let session = Arc::clone(session);
            if let Some(worker) = self.worker.take() {
                finish_worker(worker, SHUTDOWN_LIMIT);
            }
            tokio::spawn(async move {
                session.shutdown().await;
            });
        }
        emit_status(
            events,
            ProtocolId::Signal,
            AdapterStatus::Stubbed,
            "Signal linking cancelled. No session is running.",
        );
        Ok(())
    }

    fn load_chats(&mut self, events: &EventTx) -> Result<(), AdapterError> {
        match &mut self.engine {
            Engine::Sync(device) => {
                let chats = device.chats().map_err(|reason| AdapterError::Unavailable {
                    protocol: ProtocolId::Signal,
                    reason,
                })?;
                for chat in chats {
                    emit_conversation(events, chat);
                }
                Ok(())
            }
            #[cfg(feature = "signal-local")]
            Engine::Live(session) => {
                if !session.is_active() {
                    return Err(AdapterError::Unavailable {
                        protocol: ProtocolId::Signal,
                        reason: "Signal is not linked",
                    });
                }
                emit_status(
                    events,
                    ProtocolId::Signal,
                    AdapterStatus::Ready,
                    "Signal chat list is kept by the linked session.",
                );
                Ok(())
            }
        }
    }

    fn open_chat(&mut self, conversation_id: &str, events: &EventTx) -> Result<(), AdapterError> {
        match &mut self.engine {
            Engine::Sync(device) => {
                let history = device.history(conversation_id).map_err(|reason| {
                    AdapterError::Unavailable {
                        protocol: ProtocolId::Signal,
                        reason,
                    }
                })?;
                for message in history {
                    emit_message(events, message);
                }
                Ok(())
            }
            #[cfg(feature = "signal-local")]
            Engine::Live(_) => {
                let _ = conversation_id;
                emit_status(
                    events,
                    ProtocolId::Signal,
                    AdapterStatus::Ready,
                    "Signal history was loaded with the chat list.",
                );
                Ok(())
            }
        }
    }

    fn send_text(
        &mut self,
        conversation_id: &str,
        body: &str,
        request: u64,
        events: &EventTx,
    ) -> Result<(), AdapterError> {
        match &mut self.engine {
            Engine::Sync(device) => {
                match device.send(conversation_id, body) {
                    Ok(message) => {
                        emit_message(events, message);
                        crate::adapter::emit_send_accepted(
                            events,
                            ProtocolId::Signal,
                            conversation_id,
                            request,
                        );
                    }
                    Err(_) => crate::adapter::emit_send_rejected(
                        events,
                        ProtocolId::Signal,
                        conversation_id,
                        request,
                    ),
                }
                Ok(())
            }
            #[cfg(feature = "signal-local")]
            Engine::Live(session) => {
                let session = Arc::clone(session);
                let conversation_id = conversation_id.to_string();
                let body = body.to_string();
                let task_events = events.clone();
                tokio::spawn(async move {
                    let queued = session
                        .submit(live::Outbound {
                            conversation_id: conversation_id.clone(),
                            body,
                            request,
                        })
                        .await;
                    if !queued {
                        crate::adapter::emit_send_rejected(
                            &task_events,
                            ProtocolId::Signal,
                            conversation_id,
                            request,
                        );
                    }
                });
                Ok(())
            }
        }
    }
}

#[cfg_attr(not(feature = "signal-local"), allow(dead_code))]
struct SignalWorker {
    thread: std::thread::JoinHandle<()>,
    abort: tokio::task::AbortHandle,
}

/// Join the worker. If it misses `limit`, abort its task and wait until the
/// thread has ended. The sled store drops with that task.
#[cfg_attr(not(feature = "signal-local"), allow(dead_code))]
fn finish_worker(worker: SignalWorker, limit: std::time::Duration) {
    let SignalWorker { thread, abort } = worker;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = thread.join();
        let _ = tx.send(());
    });
    if rx.recv_timeout(limit).is_err() {
        abort.abort();
        let _ = rx.recv();
    }
}

#[cfg(all(test, feature = "signal-local"))]
impl SignalAdapter {
    fn install_worker_for_test(&mut self, worker: SignalWorker) {
        self.worker = Some(worker);
    }
}

impl Default for SignalAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl ProtocolAdapter for SignalAdapter {
    fn id(&self) -> ProtocolId {
        ProtocolId::Signal
    }

    fn capabilities(&self) -> ProtocolCapabilities {
        CAPABILITIES
    }

    fn start(&mut self, events: EventTx) {
        tracing::info!("signal adapter start");
        emit_status(
            &events,
            ProtocolId::Signal,
            AdapterStatus::Stubbed,
            CAPABILITIES.detail,
        );
    }

    /// Stop the worker, join it within [`SHUTDOWN_LIMIT`], then emit `Stopped`.
    fn shutdown(&mut self, events: &EventTx) {
        self.notice_accepted = false;
        #[cfg(feature = "signal-local")]
        {
            if let Engine::Live(session) = &self.engine {
                session.next_generation();
                let session = Arc::clone(session);
                let worker = self.worker.take();
                let events = events.clone();
                tokio::spawn(async move {
                    session.shutdown().await;
                    if let Some(worker) = worker {
                        finish_worker(worker, SHUTDOWN_LIMIT);
                    }
                    emit_stopped(&events, ProtocolId::Signal);
                });
            } else {
                emit_stopped(events, ProtocolId::Signal);
            }
        }
        #[cfg(not(feature = "signal-local"))]
        emit_stopped(events, ProtocolId::Signal);
    }

    fn handle(&mut self, command: AdapterCommand, events: &EventTx) -> Result<(), AdapterError> {
        match command {
            AdapterCommand::Connect {
                protocol: ProtocolId::Signal,
            } => {
                emit_status(
                    events,
                    ProtocolId::Signal,
                    AdapterStatus::Stubbed,
                    CAPABILITIES.detail,
                );
                Ok(())
            }
            AdapterCommand::Disconnect {
                protocol: ProtocolId::Signal,
            }
            | AdapterCommand::SignalCancelLink => self.cancel_link(events),
            AdapterCommand::SignalAcknowledgeNotice => self.acknowledge(events),
            AdapterCommand::SignalBeginLink => self.begin_link(events),
            AdapterCommand::LoadChats {
                protocol: ProtocolId::Signal,
            } => self.load_chats(events),
            AdapterCommand::OpenChat {
                protocol: ProtocolId::Signal,
                conversation_id,
            } => self.open_chat(&conversation_id, events),
            AdapterCommand::SendText {
                protocol: ProtocolId::Signal,
                conversation_id,
                body,
                request,
            } => self.send_text(&conversation_id, &body, request, events),
            _ => Err(AdapterError::Unavailable {
                protocol: ProtocolId::Signal,
                reason: "command is not handled by the Signal adapter",
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::adapter::AdapterEvent;
    use crate::adapter::{ChatMessage, Conversation};
    use device::FakeDevice;
    use tokio::sync::mpsc::unbounded_channel;

    fn sample_chat() -> Conversation {
        Conversation {
            protocol: ProtocolId::Signal,
            id: "chat-1".into(),
            title: "Ada".into(),
            participant: "chat-1".into(),
            preview: String::new(),
            unread: 0,
            order: 1,
            last_at: 0,
            is_group: false,
        }
    }

    fn sample_history() -> ChatMessage {
        ChatMessage {
            protocol: ProtocolId::Signal,
            conversation_id: "chat-1".into(),
            id: "signal:chat-1:1".into(),
            sender: "Ada".into(),
            body: "hello from the fixture".into(),
            outbound: false,
            delivery: crate::adapter::Delivery::Sent,
            sent_at: 0,
        }
    }

    fn fake() -> SignalAdapter {
        let mut history = HashMap::new();
        history.insert("chat-1".into(), vec![sample_history()]);
        SignalAdapter::with_device(FakeDevice::with_inbox(
            "sgnl://provision-do-not-log",
            vec![sample_chat()],
            history,
        ))
    }

    fn drain(rx: &mut tokio::sync::mpsc::UnboundedReceiver<AdapterEvent>) -> Vec<AdapterEvent> {
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    #[test]
    fn begin_link_without_notice_is_refused() {
        let (tx, mut rx) = unbounded_channel();
        let mut adapter = fake();
        let error = adapter
            .handle(AdapterCommand::SignalBeginLink, &tx)
            .expect_err("notice");
        assert!(matches!(error, AdapterError::Refused { .. }));
        let debug = format!("{error:?} {:?}", drain(&mut rx));
        assert!(!debug.contains("sgnl://"));
        assert!(!debug.contains("provision-do-not-log"));
    }

    #[test]
    fn acknowledge_does_not_mark_ready() {
        let (tx, mut rx) = unbounded_channel();
        let mut adapter = SignalAdapter::new();
        adapter
            .handle(AdapterCommand::SignalAcknowledgeNotice, &tx)
            .expect("ack");
        let events = drain(&mut rx);
        assert!(events.iter().any(|event| matches!(
            event,
            AdapterEvent::Status {
                status: AdapterStatus::Stubbed,
                ..
            }
        )));
        assert!(events.iter().all(|event| !matches!(
            event,
            AdapterEvent::Status {
                status: AdapterStatus::Ready,
                ..
            }
        )));
    }

    #[cfg(not(feature = "signal-local"))]
    #[test]
    fn feature_off_refuses_link_after_notice() {
        let (tx, mut rx) = unbounded_channel();
        let mut adapter = SignalAdapter::new();
        adapter
            .handle(AdapterCommand::SignalAcknowledgeNotice, &tx)
            .expect("ack");
        let error = adapter
            .handle(AdapterCommand::SignalBeginLink, &tx)
            .expect_err("feature off");
        assert!(matches!(error, AdapterError::Unavailable { .. }));
        let debug = format!("{error:?} {:?}", drain(&mut rx));
        assert!(!debug.contains("sgnl://"));
    }

    #[test]
    fn fake_links_lists_history_and_sends_without_logging_secrets() {
        let (tx, mut rx) = unbounded_channel();
        let mut adapter = fake();
        adapter
            .handle(AdapterCommand::SignalAcknowledgeNotice, &tx)
            .expect("ack");
        adapter
            .handle(AdapterCommand::SignalBeginLink, &tx)
            .expect("link");
        let linked = drain(&mut rx);
        let qr = linked.iter().find_map(|event| match event {
            AdapterEvent::SignalQr { code, .. } => Some(code),
            _ => None,
        });
        let qr = qr.expect("qr");
        assert_eq!(qr.reveal(), "sgnl://provision-do-not-log");
        let debug = format!("{linked:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("provision-do-not-log"));

        adapter
            .handle(
                AdapterCommand::LoadChats {
                    protocol: ProtocolId::Signal,
                },
                &tx,
            )
            .expect("chats");
        let chats = drain(&mut rx);
        assert!(chats.iter().any(|event| matches!(
            event,
            AdapterEvent::ConversationUpsert { conversation } if conversation.title == "Ada"
        )));

        adapter
            .handle(
                AdapterCommand::OpenChat {
                    protocol: ProtocolId::Signal,
                    conversation_id: "chat-1".into(),
                },
                &tx,
            )
            .expect("history");
        let history = drain(&mut rx);
        assert!(history.iter().any(|event| matches!(
            event,
            AdapterEvent::MessageReceived { message } if message.body == "hello from the fixture"
        )));

        adapter
            .handle(
                AdapterCommand::SendText {
                    protocol: ProtocolId::Signal,
                    conversation_id: "chat-1".into(),
                    body: "fixture reply".into(),
                    request: 1,
                },
                &tx,
            )
            .expect("send");
        let sent = drain(&mut rx);
        assert!(sent.iter().any(|event| matches!(
            event,
            AdapterEvent::MessageReceived { message } if message.outbound && message.body == "fixture reply"
        )));
        assert_eq!(
            sent.iter()
                .filter(|event| matches!(
                    event,
                    AdapterEvent::SendAccepted { request: 1, .. }
                        | AdapterEvent::SendRejected { request: 1, .. }
                ))
                .count(),
            1
        );
        assert!(sent.iter().any(|event| matches!(
            event,
            AdapterEvent::SendAccepted {
                request: 1,
                conversation_id,
                ..
            } if conversation_id == "chat-1"
        )));

        adapter
            .handle(
                AdapterCommand::SendText {
                    protocol: ProtocolId::Signal,
                    conversation_id: "missing".into(),
                    body: "nope".into(),
                    request: 2,
                },
                &tx,
            )
            .expect("rejected send still resolves");
        let rejected = drain(&mut rx);
        assert_eq!(
            rejected
                .iter()
                .filter(|event| matches!(
                    event,
                    AdapterEvent::SendAccepted { request: 2, .. }
                        | AdapterEvent::SendRejected { request: 2, .. }
                ))
                .count(),
            1
        );
        assert!(
            rejected
                .iter()
                .any(|event| matches!(event, AdapterEvent::SendRejected { request: 2, .. }))
        );
        let command_debug = format!("{:?}", AdapterCommand::SignalBeginLink);
        assert_eq!(command_debug, "SignalBeginLink");
        assert!(!command_debug.contains("fixture reply"));
    }

    #[test]
    fn commands_carry_no_provisioning_material() {
        assert_eq!(
            format!("{:?}", AdapterCommand::SignalBeginLink),
            "SignalBeginLink"
        );
        assert_eq!(
            format!("{:?}", AdapterCommand::SignalAcknowledgeNotice),
            "SignalAcknowledgeNotice"
        );
        assert_eq!(
            format!("{:?}", AdapterCommand::SignalCancelLink),
            "SignalCancelLink"
        );
    }

    #[test]
    fn session_file_is_under_app_data_not_the_crate() {
        let path = path::session_dir(std::path::Path::new("/var/lib/thinwire-test"));
        assert_eq!(
            path,
            std::path::PathBuf::from("/var/lib/thinwire-test/thinwire/signal")
        );
        let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        assert!(!path.starts_with(manifest));
    }

    #[cfg(unix)]
    #[test]
    fn session_dir_is_user_only() {
        let dir = std::env::temp_dir().join(format!(
            "thinwire-signal-perm-{}-{}",
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

    #[test]
    fn sources_do_not_trace_message_text_or_provisioning_urls() {
        let sources = [
            include_str!("mod.rs"),
            include_str!("device.rs"),
            include_str!("path.rs"),
            include_str!("live.rs"),
        ];
        for source in sources {
            for line in source.lines() {
                if line.contains("tracing::") {
                    assert!(
                        !line.contains("body") && !line.contains("url") && !line.contains("reveal"),
                        "tracing line must stay static: {line}"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn shutdown_reports_stopped() {
        let mut adapter = SignalAdapter::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        adapter.shutdown(&tx);
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("Stopped in time")
            .expect("channel open");
        assert_eq!(
            event,
            AdapterEvent::Stopped {
                protocol: ProtocolId::Signal
            }
        );
        let src = include_str!("mod.rs");
        assert!(src.contains("from_secs(4)"));
        let body = &src[src.find("fn shutdown(&mut self").expect("shutdown")..];
        let body = &body[..body.find("fn handle(").expect("handle")];
        let join = body.find("finish_worker").expect("join");
        let stopped = body.find("emit_stopped").expect("Stopped");
        assert!(join < stopped, "Stopped only after the worker join");
        let cancel = &src[src.find("fn cancel_link").expect("cancel")..];
        let cancel = &cancel[..cancel.find("fn load_chats").expect("load")];
        let release = cancel.find("finish_worker").expect("cancel joins");
        let status = cancel
            .find("Signal linking cancelled")
            .expect("cancelled status");
        assert!(
            release < status,
            "Cancel reports stopped only after the worker ends"
        );
    }

    #[cfg(feature = "signal-local")]
    #[tokio::test]
    async fn shutdown_joins_the_worker_before_stopped() {
        let mut adapter = SignalAdapter::new();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let _ = release_rx.recv();
        });
        let abort = tokio::spawn(async {}).abort_handle();
        adapter.install_worker_for_test(SignalWorker {
            thread: handle,
            abort,
        });
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        adapter.shutdown(&tx);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
                .await
                .is_err(),
            "Stopped waits for the worker"
        );
        release_tx.send(()).expect("release");
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("Stopped in time")
            .expect("channel open");
        assert!(matches!(
            event,
            AdapterEvent::Stopped {
                protocol: ProtocolId::Signal
            }
        ));
    }

    #[cfg(feature = "signal-local")]
    #[test]
    fn cancel_joins_the_linking_worker_before_it_returns() {
        let mut adapter = SignalAdapter::new();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let _ = release_rx.recv();
        });
        let abort = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(async { tokio::spawn(async {}).abort_handle() });
        adapter.install_worker_for_test(SignalWorker {
            thread: handle,
            abort,
        });
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            runtime.block_on(async move {
                adapter.cancel_link(&tx).expect("cancel");
            });
            done_tx.send(()).expect("done");
        });
        assert!(
            done_rx
                .recv_timeout(std::time::Duration::from_millis(200))
                .is_err(),
            "Cancel waits for the linking worker"
        );
        release_tx.send(()).expect("release");
        done_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("worker ended");
    }
}

#[cfg(test)]
mod shutdown_tests {
    use std::time::{Duration, Instant};

    use super::SignalWorker;
    use super::finish_worker;

    #[tokio::test]
    async fn missed_deadline_aborts_the_task_then_the_thread_ends() {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            let local = tokio::task::LocalSet::new();
            local.block_on(&runtime, async move {
                let task = tokio::task::spawn_local(std::future::pending::<()>());
                ready_tx.send(task.abort_handle()).expect("abort handle");
                let _ = task.await;
            });
        });
        let abort = ready_rx.recv().expect("worker started");
        let started = Instant::now();
        finish_worker(SignalWorker { thread, abort }, Duration::from_millis(30));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "abort must end the worker; Stopped waits for that"
        );
    }
}
