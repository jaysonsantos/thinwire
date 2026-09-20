//! Prove a fake adapter can push events from a tokio worker without UI APIs.

use std::time::Duration;

use thinwire::protocols::{
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
        let mut adapter = thinwire::protocols::TelegramAdapter;
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
