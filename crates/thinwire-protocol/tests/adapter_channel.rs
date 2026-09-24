//! Prove a fake adapter can push events from a tokio worker without UI APIs.

use std::time::Duration;

use thinwire_protocol::{
    AdapterCommand, AdapterEvent, AdapterStatus, FakeAdapter, ProtocolAdapter, ProtocolId,
};
use tokio::sync::mpsc::unbounded_channel;
use tokio::time::timeout;

#[tokio::test]
async fn fake_adapter_pushes_events_from_worker_without_ui_apis() {
    let (tx, mut rx) = unbounded_channel();

    let worker = tokio::spawn(async move {
        let mut adapter = FakeAdapter::new();
        adapter.start(tx.clone());
        assert!(adapter.started());
        adapter
            .handle(
                AdapterCommand::Connect {
                    protocol: ProtocolId::Telegram,
                },
                &tx,
            )
            .expect("fake connect");
        // The worker talks only to the channel. This module does not import egui/eframe.
    });

    let status = timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("status event should arrive")
        .expect("channel open");
    match status {
        AdapterEvent::Status {
            protocol,
            status,
            detail,
        } => {
            assert_eq!(protocol, ProtocolId::Telegram);
            assert_eq!(status, AdapterStatus::Ready);
            assert_eq!(detail, "fake adapter ready");
        }
        other => panic!("expected status, got {other:?}"),
    }

    let conversation = timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("conversation event should arrive")
        .expect("channel open");
    match conversation {
        AdapterEvent::ConversationUpsert { conversation } => {
            assert_eq!(conversation.id, "fake:chat");
            assert_eq!(conversation.protocol, ProtocolId::Telegram);
            assert!(conversation.last_at > 0);
        }
        other => panic!("expected conversation, got {other:?}"),
    }

    let message = timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("message event should arrive")
        .expect("channel open");
    match message {
        AdapterEvent::MessageReceived { message } => {
            assert_eq!(message.body, "event from tokio worker");
            assert_eq!(message.sender, "worker");
            assert!(message.sent_at > 0, "the fake fills the send time");
        }
        other => panic!("expected message, got {other:?}"),
    }

    worker
        .await
        .expect("worker should finish without panicking");
}

#[tokio::test]
async fn telegram_stub_emits_tdlib_status_from_worker() {
    let (tx, mut rx) = unbounded_channel();
    let worker = tokio::spawn(async move {
        let mut adapter = thinwire_protocol::TelegramAdapter::memory();
        adapter.start(tx);
    });

    let event = timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("telegram stub should emit")
        .expect("channel open");
    match event {
        AdapterEvent::Status {
            protocol, detail, ..
        } => {
            assert_eq!(protocol, ProtocolId::Telegram);
            assert!(
                detail.contains("TDLib") || detail.contains("tdlib"),
                "telegram status should mention TDLib, got {detail}"
            );
        }
        other => panic!("expected telegram status, got {other:?}"),
    }

    worker.await.expect("telegram worker");
}

#[tokio::test]
async fn telegram_unavailable_auth_emits_phases_without_secrets_on_the_wire() {
    use std::sync::Arc;

    use thinwire_protocol::{
        MemorySecretVault, TelegramAuthPhase, TelegramAuthStep, TelegramSecretKey,
        TelegramSecretVault,
    };

    let vault = Arc::new(MemorySecretVault::new());
    vault.set_secret(TelegramSecretKey::ApiId, "11111");
    vault.set_secret(TelegramSecretKey::ApiHash, "hash-value");
    vault.set_secret(TelegramSecretKey::Phone, "+15551234567");
    vault.set_secret(TelegramSecretKey::Code, "12345");

    let (tx, mut rx) = unbounded_channel();
    let worker = tokio::spawn(async move {
        let mut adapter = thinwire_protocol::TelegramAdapter::new(vault);
        adapter
            .handle(
                AdapterCommand::TelegramAuth {
                    step: TelegramAuthStep::ApiCredentials,
                    epoch: 0,
                },
                &tx,
            )
            .expect("api");
        adapter
            .handle(
                AdapterCommand::TelegramAuth {
                    step: TelegramAuthStep::Phone,
                    epoch: 0,
                },
                &tx,
            )
            .expect("phone");
        adapter
            .handle(
                AdapterCommand::TelegramAuth {
                    step: TelegramAuthStep::Code,
                    epoch: 0,
                },
                &tx,
            )
            .expect("code");
        adapter
            .handle(
                AdapterCommand::TelegramAuth {
                    step: TelegramAuthStep::TwoFactor,
                    epoch: 0,
                },
                &tx,
            )
            .expect("2fa");
    });

    let mut phases = Vec::new();
    while let Ok(Some(event)) = timeout(Duration::from_millis(200), rx.recv()).await {
        let debug = format!("{event:?}");
        assert!(!debug.contains("11111"), "{debug}");
        assert!(!debug.contains("hash-value"), "{debug}");
        assert!(!debug.contains("+15551234567"), "{debug}");
        assert!(!debug.contains("12345"), "{debug}");
        // Login phases carry the step's epoch; the host unwraps them.
        let event = match event {
            AdapterEvent::Login { event, .. } => *event,
            other => other,
        };
        if let AdapterEvent::TelegramAuth { phase } = event {
            phases.push(phase);
        }
    }

    if !cfg!(feature = "telegram-tdlib") {
        assert_eq!(
            phases,
            vec![
                TelegramAuthPhase::NeedPhone,
                TelegramAuthPhase::NeedCode,
                TelegramAuthPhase::NeedTwoFactor,
                TelegramAuthPhase::Unavailable,
            ]
        );
    }

    worker.await.expect("telegram auth worker");
}
