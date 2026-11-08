//! Tokio host: adapters run here; the UI only polls the event channel.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::runtime::Handle;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use super::adapter::LoginEpoch;
use super::{
    AdapterCommand, AdapterEvent, DiscordSecretVault, EventTx, ProtocolAdapter, ProtocolId,
    ProtocolKind, SlackSecretVault, TelegramSecretVault, WhatsAppPhoneVault, registry,
};

/// What the host needs to start the adapter of an account other than 0
/// (ADR 0014): the secret vault of that account for a protocol that runs in
/// the app, or a ready adapter (a helper adapter with its own session
/// folder).
pub enum AccountAdapter {
    Telegram(Arc<dyn TelegramSecretVault>),
    Discord(Arc<dyn DiscordSecretVault>),
    Slack(Arc<dyn SlackSecretVault>),
    Ready(Box<dyn ProtocolAdapter>),
}

/// Makes the adapter of an account other than 0. `None`: this build has no
/// such account.
pub type AccountFactory = Arc<dyn Fn(ProtocolId) -> Option<AccountAdapter> + Send + Sync>;

/// One account and its adapter. Events of an account other than 0 go
/// through a forwarder that tags them with the account.
struct Instance {
    account: ProtocolId,
    adapter: Box<dyn ProtocolAdapter>,
    events: EventTx,
}

/// Bridge between the UI thread and protocol workers.
pub struct AdapterHost {
    event_rx: UnboundedReceiver<AdapterEvent>,
    command_tx: UnboundedSender<AdapterCommand>,
    /// Telegram login epoch, shared with the Telegram adapter.
    login_epoch: LoginEpoch,
}

impl AdapterHost {
    /// Spawn the worker that owns every adapter. Safe to call off the UI thread.
    #[must_use]
    pub fn spawn(
        handle: &Handle,
        secrets: Arc<dyn TelegramSecretVault>,
        discord: Arc<dyn DiscordSecretVault>,
        slack: Arc<dyn SlackSecretVault>,
        whatsapp_phone: Arc<WhatsAppPhoneVault>,
        replacements: Vec<Box<dyn ProtocolAdapter>>,
    ) -> Self {
        Self::spawn_with_accounts(
            handle,
            secrets,
            discord,
            slack,
            whatsapp_phone,
            replacements,
            None,
        )
    }

    /// [`Self::spawn`] with a factory for the adapters of more accounts.
    /// [`AdapterCommand::AddAccount`] calls it.
    #[must_use]
    pub fn spawn_with_accounts(
        handle: &Handle,
        secrets: Arc<dyn TelegramSecretVault>,
        discord: Arc<dyn DiscordSecretVault>,
        slack: Arc<dyn SlackSecretVault>,
        whatsapp_phone: Arc<WhatsAppPhoneVault>,
        replacements: Vec<Box<dyn ProtocolAdapter>>,
        accounts: Option<AccountFactory>,
    ) -> Self {
        let login_epoch: LoginEpoch = Arc::new(AtomicU64::new(0));
        let adapter_epoch = Arc::clone(&login_epoch);
        let build_epoch = Arc::clone(&login_epoch);
        let build = accounts.map(|factory| {
            Box::new(move |account: ProtocolId| {
                let parts = factory(account)?;
                Some(build_account_adapter(account, parts, &build_epoch))
            }) as Builder
        });
        Self::spawn_inner(
            handle,
            login_epoch,
            move || {
                registry(
                    secrets,
                    discord,
                    slack,
                    whatsapp_phone,
                    adapter_epoch,
                    replacements,
                )
            },
            build,
        )
    }

    /// Spawn the worker with these adapters in place of the app's registry.
    /// For the demo adapters (#120) and tests. The routing, the login epoch,
    /// and the error handling are the same as in [`Self::spawn`].
    #[must_use]
    pub fn spawn_adapters(handle: &Handle, adapters: Vec<Box<dyn ProtocolAdapter>>) -> Self {
        Self::spawn_inner(handle, Arc::new(AtomicU64::new(0)), move || adapters, None)
    }

    /// [`Self::spawn_adapters`] with a builder for the adapters of more
    /// accounts. For tests of the account routing.
    #[doc(hidden)]
    #[must_use]
    pub fn spawn_adapters_with_accounts(
        handle: &Handle,
        adapters: Vec<Box<dyn ProtocolAdapter>>,
        accounts: impl Fn(ProtocolId) -> Option<Box<dyn ProtocolAdapter>> + Send + 'static,
    ) -> Self {
        Self::spawn_inner(
            handle,
            Arc::new(AtomicU64::new(0)),
            move || adapters,
            Some(Box::new(accounts)),
        )
    }

    fn spawn_inner(
        handle: &Handle,
        login_epoch: LoginEpoch,
        adapters: impl FnOnce() -> Vec<Box<dyn ProtocolAdapter>> + Send + 'static,
        build: Option<Builder>,
    ) -> Self {
        let (event_tx, event_rx) = unbounded_channel();
        let (command_tx, mut command_rx) = unbounded_channel::<AdapterCommand>();

        handle.spawn(async move {
            let mut instances: Vec<Instance> = adapters()
                .into_iter()
                .map(|adapter| Instance {
                    account: adapter.id(),
                    adapter,
                    events: event_tx.clone(),
                })
                .collect();
            for instance in &mut instances {
                instance.adapter.start(instance.events.clone());
            }

            while let Some(command) = command_rx.recv().await {
                let (account, command) = command.route();
                match command {
                    AdapterCommand::AddAccount { .. } => {
                        add_account(&mut instances, account, build.as_ref(), &event_tx);
                    }
                    AdapterCommand::RemoveAccount { .. } => {
                        remove_account(&mut instances, account);
                    }
                    command => {
                        let _ = dispatch(&mut instances, account, command, &event_tx);
                    }
                }
            }
        });

        Self {
            event_rx,
            command_tx,
            login_epoch,
        }
    }

    /// Non-blocking poll used by the UI frame. Does not wait on protocol I/O.
    pub fn poll_events(&mut self) -> Vec<AdapterEvent> {
        let current = self.login_epoch.load(Ordering::SeqCst);
        let mut events = Vec::new();
        while let Ok(event) = self.event_rx.try_recv() {
            if let Some(event) = deliver(event, current) {
                events.push(event);
            }
        }
        events
    }

    /// Clone of the worker command channel.
    ///
    /// Keychain hydration uses this to reconnect Discord after `spawn_blocking`
    /// finishes. Sending does not run adapter code on the caller.
    #[must_use]
    pub fn command_sender(&self) -> UnboundedSender<AdapterCommand> {
        self.command_tx.clone()
    }

    /// Split into a [`HostSender`] and the raw event receiver.
    ///
    /// A caller that waits on events (a frontend core) owns the receiver on
    /// its own task. It must pass each event through [`HostSender::deliver`]
    /// on the thread that sends commands, so the Telegram login epoch rules
    /// of `send` and `poll_events` still hold (issue #42).
    #[must_use]
    pub fn into_parts(self) -> (HostSender, UnboundedReceiver<AdapterEvent>) {
        (
            HostSender {
                command_tx: self.command_tx,
                login_epoch: self.login_epoch,
            },
            self.event_rx,
        )
    }

    /// Enqueue a command for the worker. Never runs adapter code on the caller.
    pub fn send(&self, command: AdapterCommand) {
        if !send_stamped(&self.command_tx, &self.login_epoch, command) {
            tracing::warn!("adapter host command channel closed");
        }
    }
}

/// Command side of a split [`AdapterHost`]. It keeps the Telegram login
/// epoch rules: `send` stamps and bumps like [`AdapterHost::send`], and
/// `deliver` unwraps and drops like [`AdapterHost::poll_events`].
#[derive(Debug, Clone)]
pub struct HostSender {
    command_tx: UnboundedSender<AdapterCommand>,
    login_epoch: LoginEpoch,
}

impl HostSender {
    /// Enqueue a command for the worker. False when the worker is gone.
    pub fn send(&self, command: AdapterCommand) -> bool {
        send_stamped(&self.command_tx, &self.login_epoch, command)
    }

    /// Unwrap a stamped login event with the epoch of now; `None` for a stale
    /// one. Call it on the thread that calls `send`, just before the event is
    /// applied, so a Cancel sent before this call drops the event.
    #[must_use]
    pub fn deliver(&self, event: AdapterEvent) -> Option<AdapterEvent> {
        deliver(event, self.login_epoch.load(Ordering::SeqCst))
    }

    /// Test hook: a sender over a channel the test reads, with its own epoch.
    #[doc(hidden)]
    #[must_use]
    pub fn for_test(command_tx: UnboundedSender<AdapterCommand>) -> Self {
        Self {
            command_tx,
            login_epoch: Arc::new(AtomicU64::new(0)),
        }
    }
}

/// Stamp first, then bump. A step sent before Cancel keeps the old epoch; the
/// adapter ignores it instead of emitting `Failed` after the secrets were
/// cleared. The bump happens on the sending thread, before the next delivery:
/// login events that an old client already queued are then stale (issue #42).
fn send_stamped(
    command_tx: &UnboundedSender<AdapterCommand>,
    login_epoch: &LoginEpoch,
    mut command: AdapterCommand,
) -> bool {
    let epoch = login_epoch.load(Ordering::SeqCst);
    stamp_auth_epoch(&mut command, epoch);
    if ends_telegram_login(&command) {
        login_epoch.fetch_add(1, Ordering::SeqCst);
    }
    command_tx.send(command).is_ok()
}

/// The error status of a failed login step carries the step's epoch, so a
/// Cancel that happens after the step started still drops it (PR #49 review).
fn stamp_login_failure(auth_epoch: Option<u64>, status: AdapterEvent) -> AdapterEvent {
    match auth_epoch {
        Some(epoch) => AdapterEvent::Login {
            epoch,
            event: Box::new(status),
        },
        None => status,
    }
}

/// Record the login epoch a Telegram step was sent under.
fn stamp_auth_epoch(command: &mut AdapterCommand, epoch: u64) {
    match command {
        AdapterCommand::TelegramAuth { epoch: slot, .. } => *slot = epoch,
        // The login step of another Telegram account (ADR 0014).
        AdapterCommand::ToAccount { command, .. } => stamp_auth_epoch(command, epoch),
        _ => {}
    }
}

/// Commands that end the login flow of a Telegram client. Every Telegram
/// account shares the login epoch.
fn ends_telegram_login(command: &AdapterCommand) -> bool {
    matches!(
        command,
        AdapterCommand::Disconnect { protocol } | AdapterCommand::Shutdown { protocol }
            if protocol.is(ProtocolKind::Telegram)
    )
}

/// Unwrap a stamped login event if its epoch is current; drop a stale one.
/// An event of another account keeps its account. Other events pass
/// unchanged.
fn deliver(event: AdapterEvent, current_epoch: u64) -> Option<AdapterEvent> {
    match event {
        AdapterEvent::Login { epoch, event } => (epoch == current_epoch).then_some(*event),
        AdapterEvent::FromAccount { account, event } => {
            deliver(*event, current_epoch).map(|inner| inner.for_account(account))
        }
        other => Some(other),
    }
}

/// Builds the adapter of one more account.
type Builder = Box<dyn Fn(ProtocolId) -> Option<Box<dyn ProtocolAdapter>> + Send>;

/// The adapter of an account other than 0, from its parts.
fn build_account_adapter(
    account: ProtocolId,
    parts: AccountAdapter,
    epoch: &LoginEpoch,
) -> Box<dyn ProtocolAdapter> {
    match parts {
        AccountAdapter::Telegram(vault) => Box::new(super::TelegramAdapter::with_login_epoch(
            vault,
            super::telegram::TelegramApiSource::from_build(),
            Arc::clone(epoch),
        )),
        AccountAdapter::Discord(vault) => Box::new(super::DiscordAdapter::new(vault)),
        AccountAdapter::Slack(vault) => super::slack::registry_adapter_for(vault, account.slot()),
        AccountAdapter::Ready(adapter) => adapter,
    }
}

/// Start the adapter of `account`. Account 0, an account that runs, and a
/// build with no factory start nothing.
fn add_account(
    instances: &mut Vec<Instance>,
    account: ProtocolId,
    build: Option<&Builder>,
    main: &EventTx,
) {
    if account.slot() == 0 || instances.iter().any(|instance| instance.account == account) {
        return;
    }
    let Some(mut adapter) = build.and_then(|build| build(account)) else {
        tracing::warn!(%account, "this build cannot start another account");
        return;
    };
    let events = forwarder(account, main.clone());
    adapter.start(events.clone());
    instances.push(Instance {
        account,
        adapter,
        events,
    });
}

/// Shut the adapter of `account` down and drop it. Account 0 stays.
fn remove_account(instances: &mut Vec<Instance>, account: ProtocolId) {
    if account.slot() == 0 {
        return;
    }
    if let Some(at) = instances
        .iter()
        .position(|instance| instance.account == account)
    {
        let mut instance = instances.remove(at);
        instance.adapter.shutdown(&instance.events);
    }
}

/// A sender for the adapter of `account`: each event gets the account
/// before it goes to `main`. The order of the events stays.
fn forwarder(account: ProtocolId, main: EventTx) -> EventTx {
    let (tx, mut rx) = unbounded_channel::<AdapterEvent>();
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            if main.send(event.for_account(account)).is_err() {
                break;
            }
        }
    });
    tx
}

/// Route one command to the adapter of `account`. Returns true when the
/// adapter failed it in `handle`; the host then sends that failure as a
/// `Status` event.
fn dispatch(
    instances: &mut [Instance],
    account: ProtocolId,
    command: AdapterCommand,
    main: &EventTx,
) -> bool {
    let Some(instance) = instances
        .iter_mut()
        .find(|instance| instance.account == account)
    else {
        // An account with no adapter, for example a helper that this build
        // cannot start: it is stopped already, so the close does not wait.
        if matches!(command, AdapterCommand::Shutdown { .. }) {
            let _ = main.send(AdapterEvent::Stopped { protocol: account });
        }
        tracing::warn!(%account, "no adapter registered");
        return false;
    };
    dispatch_to(&mut instance.adapter, command, &instance.events)
}

/// [`dispatch`] over a plain list of account-0 adapters, for the contract
/// kit and the tests.
#[cfg(any(test, feature = "contract-kit"))]
pub(crate) fn dispatch_adapters(
    adapters: &mut [Box<dyn ProtocolAdapter>],
    command: AdapterCommand,
    events: &EventTx,
) -> bool {
    let (account, command) = command.route();
    let protocol = account.first_account();
    let Some(adapter) = adapters.iter_mut().find(|adapter| adapter.id() == protocol) else {
        tracing::warn!(%protocol, "no adapter registered");
        return false;
    };
    dispatch_to(adapter, command, events)
}

/// Hand one command, as its adapter sees it, to `adapter`.
fn dispatch_to(
    adapter: &mut Box<dyn ProtocolAdapter>,
    command: AdapterCommand,
    events: &EventTx,
) -> bool {
    // Every adapter answers Shutdown, so the app can wait for all of them.
    if matches!(command, AdapterCommand::Shutdown { .. }) {
        adapter.shutdown(events);
        return false;
    }
    // The viewed chat is a hint to the adapter, not a request: route it to
    // the trait method, so an adapter with no read state needs no arm.
    if let AdapterCommand::ViewChat {
        conversation_id, ..
    } = &command
    {
        adapter.view_chat(conversation_id.as_deref(), events);
        return false;
    }
    if let AdapterCommand::ViewTopic {
        conversation_id,
        topic,
        ..
    } = &command
    {
        adapter.view_topic(conversation_id, *topic, events);
        return false;
    }
    let auth_epoch = match command {
        AdapterCommand::TelegramAuth { epoch, .. } => Some(epoch),
        _ => None,
    };
    if let Err(error) = adapter.handle(command, events) {
        tracing::info!(%error, "adapter refused or failed a command");
        let status = AdapterEvent::Status {
            protocol: error_protocol(&error),
            status: match &error {
                super::AdapterError::Refused { .. } => super::AdapterStatus::Refused,
                super::AdapterError::Unavailable { .. } => super::AdapterStatus::Error,
            },
            detail: error.to_string(),
        };
        let _ = events.send(stamp_login_failure(auth_epoch, status));
        return true;
    }
    false
}

fn error_protocol(error: &super::AdapterError) -> super::ProtocolId {
    match *error {
        super::AdapterError::Refused { protocol, .. }
        | super::AdapterError::Unavailable { protocol, .. } => protocol,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TelegramAuthPhase;

    fn stamped(epoch: u64, phase: TelegramAuthPhase) -> AdapterEvent {
        AdapterEvent::Login {
            epoch,
            event: Box::new(AdapterEvent::TelegramAuth { phase }),
        }
    }

    #[test]
    fn an_auth_step_is_stamped_with_the_epoch_it_was_sent_under() {
        use crate::TelegramAuthStep;
        let mut step = AdapterCommand::TelegramAuth {
            step: TelegramAuthStep::Phone,
            epoch: 99,
        };
        stamp_auth_epoch(&mut step, 0);
        assert!(matches!(
            step,
            AdapterCommand::TelegramAuth {
                step: TelegramAuthStep::Phone,
                epoch: 0
            }
        ));
    }

    #[test]
    fn a_login_event_queued_before_cancel_is_dropped_at_delivery() {
        // A fake old client queued NeedPhone and Ready under epoch 0. Then the
        // user pressed Cancel: the host sent Disconnect and bumped to 1.
        let cancel = AdapterCommand::Disconnect {
            protocol: ProtocolId::Telegram,
        };
        assert!(ends_telegram_login(&cancel));
        let current = 1;
        assert_eq!(
            deliver(stamped(0, TelegramAuthPhase::NeedPhone), current),
            None
        );
        assert_eq!(deliver(stamped(0, TelegramAuthPhase::Ready), current), None);
        // The new client's events (epoch 1) arrive unwrapped.
        assert_eq!(
            deliver(stamped(1, TelegramAuthPhase::NeedPhone), current),
            Some(AdapterEvent::TelegramAuth {
                phase: TelegramAuthPhase::NeedPhone
            })
        );
        // Other events are never stamped and always pass.
        let reset = AdapterEvent::TelegramDataReset {
            moved_to: "tdlib.stale-1".into(),
        };
        assert_eq!(deliver(reset.clone(), current), Some(reset));
        assert!(!ends_telegram_login(&AdapterCommand::LoadChats {
            protocol: ProtocolId::Telegram
        }));
        assert!(!ends_telegram_login(&AdapterCommand::Disconnect {
            protocol: ProtocolId::WhatsApp
        }));
    }

    #[test]
    fn a_failed_step_status_is_stamped_and_dropped_after_cancel() {
        let status = AdapterEvent::Status {
            protocol: ProtocolId::Telegram,
            status: crate::AdapterStatus::Error,
            detail: "telegram phone is missing from the secret store".into(),
        };
        let stamped = stamp_login_failure(Some(0), status.clone());
        // Cancel bumped the epoch to 1 before the UI polled.
        assert_eq!(
            deliver(stamped.clone(), 1),
            None,
            "no failure on a cancelled flow"
        );
        assert_eq!(
            deliver(stamped, 0),
            Some(status.clone()),
            "shown when still current"
        );
        assert_eq!(
            stamp_login_failure(None, status.clone()),
            status,
            "other commands unchanged"
        );
    }

    #[tokio::test]
    async fn the_host_bumps_the_epoch_on_the_ui_thread_when_it_sends_cancel() {
        let vault = Arc::new(crate::MemorySecretVault::new());
        let host = AdapterHost::spawn(
            &Handle::current(),
            Arc::clone(&vault) as Arc<dyn TelegramSecretVault>,
            Arc::new(crate::MemoryDiscordVault::new()) as Arc<dyn DiscordSecretVault>,
            Arc::new(crate::MemorySlackVault::new()) as Arc<dyn SlackSecretVault>,
            Arc::new(WhatsAppPhoneVault::new()),
            Vec::new(),
        );
        assert_eq!(host.login_epoch.load(Ordering::SeqCst), 0);
        host.send(AdapterCommand::Disconnect {
            protocol: ProtocolId::Telegram,
        });
        assert_eq!(
            host.login_epoch.load(Ordering::SeqCst),
            1,
            "bumped at send time, before the worker handles the command"
        );
    }

    #[test]
    fn a_split_host_sender_keeps_the_login_epoch_rules() {
        let (tx, mut rx) = unbounded_channel();
        let sender = HostSender::for_test(tx);
        let queued_before_cancel = stamped(0, TelegramAuthPhase::Ready);
        assert!(sender.send(AdapterCommand::Disconnect {
            protocol: ProtocolId::Telegram,
        }));
        assert_eq!(sender.deliver(queued_before_cancel), None);
        assert_eq!(
            sender.deliver(stamped(1, TelegramAuthPhase::NeedPhone)),
            Some(AdapterEvent::TelegramAuth {
                phase: TelegramAuthPhase::NeedPhone
            })
        );
        // A step sent now carries the new epoch.
        assert!(sender.send(AdapterCommand::TelegramAuth {
            step: crate::TelegramAuthStep::Phone,
            epoch: 99,
        }));
        let _disconnect = rx.try_recv().expect("disconnect");
        assert!(matches!(
            rx.try_recv().expect("step"),
            AdapterCommand::TelegramAuth { epoch: 1, .. }
        ));
    }

    /// `AddAccount` starts a second adapter. Its events carry its account,
    /// and a command for that account reaches it as account 0. Account 0
    /// keeps its own adapter. `RemoveAccount` shuts it down.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_account_has_its_own_adapter_and_tagged_events() {
        let second = ProtocolId::account(crate::ProtocolKind::Telegram, 1).expect("slot");
        let mut host = AdapterHost::spawn_adapters_with_accounts(
            &Handle::current(),
            vec![Box::new(crate::FakeAdapter::default())],
            |account| {
                (account.slot() == 1)
                    .then(|| Box::new(crate::FakeAdapter::default()) as Box<dyn ProtocolAdapter>)
            },
        );
        host.send(AdapterCommand::AddAccount { protocol: second });
        host.send(AdapterCommand::Connect { protocol: second });
        let mut seen = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline
            && !seen.iter().any(|event| {
                matches!(event, AdapterEvent::ConversationUpsert { conversation } if conversation.protocol == second)
            })
        {
            seen.extend(host.poll_events());
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(seen.iter().any(|event| matches!(
            event,
            AdapterEvent::Account { protocol, state: crate::AccountState::Linked } if *protocol == second
        )), "the second adapter started: {seen:?}");
        assert!(
            seen.iter().any(|event| matches!(
                event,
                AdapterEvent::Account {
                    protocol: ProtocolId::Telegram,
                    ..
                }
            )),
            "account 0 has its own adapter: {seen:?}"
        );
        assert!(
            seen.iter().any(|event| matches!(
                event,
                AdapterEvent::ConversationUpsert { conversation } if conversation.protocol == second
            )),
            "the Connect of the second account reached its adapter: {seen:?}"
        );
        assert!(
            !seen.iter().any(|event| matches!(
                event,
                AdapterEvent::ConversationUpsert { conversation } if conversation.protocol == ProtocolId::Telegram
            )),
            "the Connect did not reach account 0: {seen:?}"
        );
        host.send(AdapterCommand::RemoveAccount { protocol: second });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut stopped = false;
        while std::time::Instant::now() < deadline && !stopped {
            stopped = host.poll_events().iter().any(
                |event| matches!(event, AdapterEvent::Stopped { protocol } if *protocol == second),
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(stopped, "the second adapter stopped");
    }

    #[test]
    fn view_chat_reaches_the_default_method_and_emits_nothing() {
        let mut adapters: Vec<Box<dyn ProtocolAdapter>> =
            vec![Box::new(crate::FakeAdapter::default())];
        let (tx, mut rx) = unbounded_channel();
        for conversation_id in [Some("telegram:1".to_owned()), None] {
            dispatch_adapters(
                &mut adapters,
                AdapterCommand::ViewChat {
                    protocol: ProtocolId::Telegram,
                    conversation_id,
                },
                &tx,
            );
        }
        assert!(
            rx.try_recv().is_err(),
            "no status, no error for a viewed-chat hint"
        );
    }
}
