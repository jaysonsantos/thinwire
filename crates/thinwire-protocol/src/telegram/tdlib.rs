//! Live `tdlib-rs` client. Compiled only with `--features telegram-tdlib`.
//!
//! FFI and network I/O stay on a dedicated receive thread plus tokio tasks.
//! Credential values are read from the vault and never logged.

use std::path::PathBuf;
use std::sync::Arc;
use std::thread;

use tokio::sync::mpsc::UnboundedSender;

use super::credentials::{TelegramApiSource, parse_resolved_api_id, require_resolved_api};
use crate::adapter::{
    AdapterStatus, ChatMessage, Conversation, EventTx, ProtocolId, TelegramAuthPhase,
    TelegramAuthStep, emit_conversation, emit_message, emit_status, emit_telegram_auth,
};
use crate::secrets::{TelegramSecretKey, TelegramSecretVault};

/// Marker written to the persistent session key after authorization.
pub const TDLIB_SESSION_MARKER: &str = "tdlib-ready";

#[derive(Clone, Copy)]
enum TdlibCommand {
    Step(TelegramAuthStep),
}

/// Owns the TDLib client id and the command sink into the worker.
pub struct TdlibRuntime {
    commands: Option<UnboundedSender<TdlibCommand>>,
}

impl TdlibRuntime {
    #[must_use]
    pub const fn new() -> Self {
        Self { commands: None }
    }

    pub fn submit(
        &mut self,
        step: TelegramAuthStep,
        secrets: Arc<dyn TelegramSecretVault>,
        source: TelegramApiSource,
        events: &EventTx,
    ) {
        let sent = super::send_or_respawn(&mut self.commands, TdlibCommand::Step(step), || {
            spawn_tdlib_worker(Arc::clone(&secrets), source.clone(), events.clone())
        });
        if !sent {
            emit_status(
                events,
                ProtocolId::Telegram,
                AdapterStatus::Error,
                "TDLib worker is not running. Cancel and try again.",
            );
        }
    }
}

impl Default for TdlibRuntime {
    fn default() -> Self {
        Self::new()
    }
}

fn spawn_tdlib_worker(
    secrets: Arc<dyn TelegramSecretVault>,
    source: TelegramApiSource,
    events: EventTx,
) -> UnboundedSender<TdlibCommand> {
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let (update_tx, mut update_rx) = tokio::sync::mpsc::unbounded_channel();

    let client_id = tdlib_rs::create_client();
    thread::Builder::new()
        .name("thinwire-tdlib-recv".into())
        .spawn(move || {
            while let Some((update, id)) = tdlib_rs::receive() {
                if id == client_id {
                    let _ = update_tx.send(update);
                }
            }
        })
        .ok();

    tokio::spawn(async move {
        if tdlib_rs::functions::set_log_verbosity_level(1, client_id)
            .await
            .is_err()
        {
            tracing::info!("tdlib log verbosity was not applied");
        }

        loop {
            tokio::select! {
                command = cmd_rx.recv() => {
                    let Some(TdlibCommand::Step(step)) = command else {
                        break;
                    };
                    apply_step(client_id, step, secrets.as_ref(), &events).await;
                }
                update = update_rx.recv() => {
                    let Some(update) = update else {
                        break;
                    };
                    apply_update(client_id, update, secrets.as_ref(), &source, &events).await;
                }
            }
        }
    });

    cmd_tx
}

async fn apply_step(
    client_id: i32,
    step: TelegramAuthStep,
    secrets: &dyn TelegramSecretVault,
    events: &EventTx,
) {
    match step {
        TelegramAuthStep::ApiCredentials => {
            emit_status(
                events,
                ProtocolId::Telegram,
                AdapterStatus::Connecting,
                "TDLib client started. Waiting for authorization state.",
            );
        }
        TelegramAuthStep::Phone => {
            let Some(phone) = secrets.get_secret(TelegramSecretKey::Phone) else {
                emit_status(
                    events,
                    ProtocolId::Telegram,
                    AdapterStatus::Error,
                    "Phone number is missing from the secret store.",
                );
                return;
            };
            if tdlib_rs::functions::set_authentication_phone_number(phone, None, client_id)
                .await
                .is_err()
            {
                emit_status(
                    events,
                    ProtocolId::Telegram,
                    AdapterStatus::Error,
                    "Telegram rejected the phone number. Check the number or Cancel.",
                );
            }
        }
        TelegramAuthStep::Code => {
            let Some(code) = secrets.get_secret(TelegramSecretKey::Code) else {
                emit_status(
                    events,
                    ProtocolId::Telegram,
                    AdapterStatus::Error,
                    "Login code is missing from the secret store.",
                );
                return;
            };
            if tdlib_rs::functions::check_authentication_code(code, client_id)
                .await
                .is_err()
            {
                emit_status(
                    events,
                    ProtocolId::Telegram,
                    AdapterStatus::Error,
                    "Telegram rejected the login code. Try again or Cancel.",
                );
            }
        }
        TelegramAuthStep::TwoFactor | TelegramAuthStep::Complete => {
            let password = secrets
                .get_secret(TelegramSecretKey::Password)
                .unwrap_or_default();
            if password.is_empty() {
                return;
            }
            if tdlib_rs::functions::check_authentication_password(password, client_id)
                .await
                .is_err()
            {
                emit_status(
                    events,
                    ProtocolId::Telegram,
                    AdapterStatus::Error,
                    "Telegram rejected the 2FA password. Try again or Cancel.",
                );
            }
        }
    }
}

async fn apply_update(
    client_id: i32,
    update: tdlib_rs::enums::Update,
    secrets: &dyn TelegramSecretVault,
    source: &TelegramApiSource,
    events: &EventTx,
) {
    let tdlib_rs::enums::Update::AuthorizationState(state) = update else {
        return;
    };
    match state.authorization_state {
        tdlib_rs::enums::AuthorizationState::WaitTdlibParameters => {
            set_parameters(client_id, secrets, source, events).await;
        }
        tdlib_rs::enums::AuthorizationState::WaitPhoneNumber => {
            emit_telegram_auth(events, TelegramAuthPhase::NeedPhone);
            emit_status(
                events,
                ProtocolId::Telegram,
                AdapterStatus::Connecting,
                "Telegram needs a phone number. Values stay in the secret store.",
            );
        }
        tdlib_rs::enums::AuthorizationState::WaitCode(_) => {
            emit_telegram_auth(events, TelegramAuthPhase::NeedCode);
            emit_status(
                events,
                ProtocolId::Telegram,
                AdapterStatus::Connecting,
                "Telegram sent a login code. Enter it here. It is not logged.",
            );
        }
        tdlib_rs::enums::AuthorizationState::WaitPassword(_) => {
            emit_telegram_auth(events, TelegramAuthPhase::NeedTwoFactor);
            emit_status(
                events,
                ProtocolId::Telegram,
                AdapterStatus::Connecting,
                "Telegram needs the optional 2FA password. Leave blank only if this account has none.",
            );
        }
        tdlib_rs::enums::AuthorizationState::Ready => {
            secrets.set_secret(TelegramSecretKey::Session, TDLIB_SESSION_MARKER);
            emit_telegram_auth(events, TelegramAuthPhase::Ready);
            emit_status(
                events,
                ProtocolId::Telegram,
                AdapterStatus::Ready,
                "Telegram is ready. TDLib session is live.",
            );
            emit_conversation(
                events,
                Conversation {
                    protocol: ProtocolId::Telegram,
                    id: "telegram:ready".into(),
                    title: "Telegram".into(),
                    participant: "you".into(),
                    preview: "Connected via TDLib.".into(),
                    unread: 0,
                },
            );
            emit_message(
                events,
                ChatMessage {
                    protocol: ProtocolId::Telegram,
                    conversation_id: "telegram:ready".into(),
                    id: "telegram:ready:1".into(),
                    sender: "thinwire".into(),
                    body: "Telegram login finished on the official TDLib path.".into(),
                    outbound: false,
                },
            );
        }
        tdlib_rs::enums::AuthorizationState::WaitEmailAddress(_)
        | tdlib_rs::enums::AuthorizationState::WaitEmailCode(_)
        | tdlib_rs::enums::AuthorizationState::WaitRegistration(_)
        | tdlib_rs::enums::AuthorizationState::WaitPremiumPurchase(_)
        | tdlib_rs::enums::AuthorizationState::WaitOtherDeviceConfirmation(_) => {
            emit_status(
                events,
                ProtocolId::Telegram,
                AdapterStatus::Error,
                "Telegram asked for a login step thinwire does not support yet. Cancel and use another client.",
            );
        }
        tdlib_rs::enums::AuthorizationState::LoggingOut
        | tdlib_rs::enums::AuthorizationState::Closing
        | tdlib_rs::enums::AuthorizationState::Closed => {
            emit_status(
                events,
                ProtocolId::Telegram,
                AdapterStatus::Stubbed,
                "Telegram session closed.",
            );
        }
    }
}

async fn set_parameters(
    client_id: i32,
    secrets: &dyn TelegramSecretVault,
    source: &TelegramApiSource,
    events: &EventTx,
) {
    let (api_id, api_hash) = match require_resolved_api(secrets, source) {
        Ok(pair) => pair,
        Err(_) => {
            emit_status(
                events,
                ProtocolId::Telegram,
                AdapterStatus::Error,
                "telegram api credentials are missing; set a keychain override or rebuild with TELEGRAM_API_ID",
            );
            return;
        }
    };
    let Ok(api_id) = parse_resolved_api_id(&api_id) else {
        emit_status(
            events,
            ProtocolId::Telegram,
            AdapterStatus::Error,
            "telegram api_id must be a number",
        );
        return;
    };
    let database_directory = tdlib_data_dir().display().to_string();
    let encryption_key = ensure_db_key(secrets);
    if tdlib_rs::functions::set_tdlib_parameters(
        false,
        database_directory,
        String::new(),
        encryption_key,
        true,
        true,
        true,
        false,
        api_id,
        api_hash,
        "en".into(),
        "thinwire".into(),
        String::new(),
        env!("CARGO_PKG_VERSION").into(),
        client_id,
    )
    .await
    .is_err()
    {
        emit_status(
            events,
            ProtocolId::Telegram,
            AdapterStatus::Error,
            "TDLib rejected the client parameters. Check api_id and api_hash.",
        );
    }
}

fn ensure_db_key(vault: &dyn TelegramSecretVault) -> String {
    if let Some(existing) = vault.get_secret(TelegramSecretKey::DbEncryption)
        && !existing.is_empty()
    {
        return existing;
    }
    let key = generate_db_key();
    vault.set_secret(TelegramSecretKey::DbEncryption, &key);
    key
}

fn generate_db_key() -> String {
    super::db_key::generate_db_key()
}

fn tdlib_data_dir() -> PathBuf {
    let path = if let Some(dir) = std::env::var_os("THINWIRE_TDLIB_DIR") {
        PathBuf::from(dir)
    } else {
        let mut base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(|home| PathBuf::from(home).join(".local").join("share"))
            })
            .unwrap_or_else(std::env::temp_dir);
        base.push("thinwire");
        base.push("tdlib");
        base
    };
    let _ = std::fs::create_dir_all(&path);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700));
    }
    path
}
