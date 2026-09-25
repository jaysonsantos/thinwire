//! Protocol adapters, capability metadata, and the tokio host.

mod adapter;
mod discord;
mod fake;
mod host;
mod risk;
mod secrets;
mod signal;
mod slack;
mod telegram;
mod whatsapp;

pub use adapter::{
    AccountState, AdapterCommand, AdapterError, AdapterEvent, AdapterStatus, ChatMessage,
    Conversation, Delivery, DiscordAuthMode, EventTx, ProtocolAdapter, ProtocolCapabilities,
    ProtocolId, RedactedPairingSecret, SupportClass, TelegramAuthError, TelegramAuthPhase,
    TelegramAuthStep, TelegramCodeVia, emit_account, emit_chat_list_loaded, emit_conversation,
    emit_history_loaded, emit_message, emit_send_accepted, emit_send_rejected, emit_status,
    emit_stopped,
};
pub use discord::{
    DISCORD_SECRET_BOT_TOKEN, DISCORD_SECRET_SERVICE, DiscordAdapter, DiscordOAuthInstall,
    DiscordSecretVault, MemoryDiscordVault,
};
pub use fake::FakeAdapter;
pub use host::{AdapterHost, HostSender};
pub use risk::{
    CRITIC_BULLET_1, CRITIC_BULLET_2, CRITIC_BULLET_3, CRITIC_RISK_BULLETS, critic_bullets_for,
    requires_experimental_gate,
};
pub use secrets::{
    MemorySecretVault, TDLIB_FOLDER, TDLIB_KEYUTILS_FOLDER, TELEGRAM_SECRET_API_HASH,
    TELEGRAM_SECRET_API_ID, TELEGRAM_SECRET_CODE, TELEGRAM_SECRET_DB_KEY, TELEGRAM_SECRET_PASSWORD,
    TELEGRAM_SECRET_PHONE, TELEGRAM_SECRET_SERVICE, TELEGRAM_SECRET_SESSION, TelegramSecretKey,
    TelegramSecretVault,
};
pub use signal::SignalAdapter;
pub use slack::{
    MemorySlackVault, SLACK_CONVERSATION_PREFIX, SLACK_OAUTH_CALLBACK_PATH,
    SLACK_OAUTH_LOOPBACK_PORT, SLACK_SECRET_SERVICE, SlackAdapter, SlackApiError, SlackApiOrigin,
    SlackApiSource, SlackAppToken, SlackBotToken, SlackBrowser, SlackCallbackError, SlackChannel,
    SlackChannelKind, SlackChannelPage, SlackCodeExchange, SlackDeps, SlackEventSource,
    SlackEventStream, SlackInbound, SlackInbox, SlackInstallGrant, SlackInstalledWorkspace,
    SlackLoopback, SlackPost, SlackSecretKey, SlackSecretVault, SlackWebApi, WORKSPACE_BOT_SCOPES,
    authorize_url, loopback_redirect_uri, new_oauth_state, parse_loopback_callback,
    resolve_slack_app_token, resolve_slack_client,
};
#[cfg(feature = "slack-oauth")]
pub use slack::{oauth_v2_access_request, socket_mode_config, workspace_bot_token};
pub use telegram::{
    TelegramAdapter, TelegramApiOrigin, TelegramApiSource, parse_telegram_chat_id,
    resolve_telegram_api, telegram_api_available,
};
pub use whatsapp::{WhatsAppAdapter, WhatsAppPhoneVault};

/// Shell protocols in display order. Signal is local-only and hidden unless `signal-local` is on.
pub fn catalog() -> [ProtocolCapabilities; 5] {
    [
        telegram::TelegramAdapter::capabilities(),
        whatsapp::WhatsAppAdapter::capabilities(),
        discord::DiscordAdapter::capabilities(),
        slack::SlackAdapter::capabilities(),
        signal::SignalAdapter::capabilities(),
    ]
}

pub(crate) fn registry(
    secrets: std::sync::Arc<dyn TelegramSecretVault>,
    discord: std::sync::Arc<dyn DiscordSecretVault>,
    slack: std::sync::Arc<dyn SlackSecretVault>,
    whatsapp_phone: std::sync::Arc<WhatsAppPhoneVault>,
    login_epoch: adapter::LoginEpoch,
    signal: Option<Box<dyn ProtocolAdapter>>,
) -> Vec<Box<dyn ProtocolAdapter>> {
    let mut adapters: Vec<Box<dyn ProtocolAdapter>> = vec![
        Box::new(TelegramAdapter::with_login_epoch(
            secrets,
            crate::telegram::TelegramApiSource::from_build(),
            login_epoch,
        )),
        Box::new(WhatsAppAdapter::new(whatsapp_phone)),
        Box::new(DiscordAdapter::new(discord)),
        slack::registry_adapter(slack),
        Box::new(signal::SignalAdapter::new()),
    ];
    if let Some(signal) = signal
        && let Some(slot) = adapters
            .iter_mut()
            .find(|adapter| adapter.id() == ProtocolId::Signal)
    {
        *slot = signal;
    }
    adapters
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_lists_v1_four_protocols_with_honest_support() {
        let caps = catalog();
        let ids: Vec<ProtocolId> = caps.iter().map(|c| c.id).collect();
        assert_eq!(
            ids,
            vec![
                ProtocolId::Telegram,
                ProtocolId::WhatsApp,
                ProtocolId::Discord,
                ProtocolId::Slack,
                ProtocolId::Signal,
            ]
        );
        assert_eq!(ProtocolId::ALL.as_slice(), ids.as_slice());
        assert_eq!(caps.len(), 5);

        let by_id = |id| caps.iter().find(|c| c.id == id).expect("protocol");
        assert_eq!(by_id(ProtocolId::Telegram).support, SupportClass::Supported);
        assert!(by_id(ProtocolId::Telegram).official_api);
        assert_eq!(by_id(ProtocolId::Slack).support, SupportClass::Supported);
        assert!(by_id(ProtocolId::Slack).official_api);
        assert_eq!(
            by_id(ProtocolId::WhatsApp).support,
            SupportClass::Experimental
        );
        assert!(!by_id(ProtocolId::WhatsApp).official_api);
        assert_eq!(
            by_id(ProtocolId::Discord).support,
            SupportClass::Constrained
        );
        assert!(!by_id(ProtocolId::Discord).allows_user_account_automation);
        assert_eq!(
            by_id(ProtocolId::Signal).support,
            SupportClass::Experimental
        );
        assert!(!by_id(ProtocolId::Signal).official_api);
        assert!(
            by_id(ProtocolId::Signal).detail.contains("signal-local")
                || by_id(ProtocolId::Signal).detail.contains("presage")
        );
    }

    #[test]
    fn experimental_labels_avoid_banned_marketing_words() {
        for caps in catalog() {
            let short = caps.short_label;
            let detail = caps.detail;
            let blob = format!("{short} {detail}").to_ascii_lowercase();
            if matches!(
                caps.id,
                ProtocolId::WhatsApp | ProtocolId::Discord | ProtocolId::Signal
            ) {
                let name = caps.id.display_name();
                assert!(!contains_word(&blob, "reliable"), "{name}");
                assert!(!contains_word(&blob, "production"), "{name}");
                assert!(!contains_word(&blob, "official"), "{name}");
            }
        }
    }

    #[test]
    fn v1_does_not_depend_on_agpl_signal_client_or_presage() {
        let protocol = include_str!("../Cargo.toml");
        for name in [
            "presage",
            "libsignal",
            "libsignal-service",
            "wacore-libsignal",
        ] {
            assert!(
                !protocol.contains(name),
                "thinwire-protocol must not name {name}"
            );
        }
        let app = include_str!("../../thinwire/Cargo.toml");
        assert!(
            app.lines().any(|line| line.trim() == "default = []"),
            "the default feature set stays empty"
        );
        let release = include_str!("../../../.github/workflows/os-zips.yml");
        assert!(release.contains("--features telegram-tdlib"));
        for name in [
            "signal-local",
            "whatsapp-web",
            "presage",
            "libsignal",
            "libsignal-service",
            "wacore-libsignal",
        ] {
            assert!(
                !release.contains(name),
                "release builds must not name {name}"
            );
        }
        for (label, args) in [
            ("default features", &[][..]),
            ("telegram-tdlib", &["--features", "telegram-tdlib"][..]),
        ] {
            let tree = release_cargo_tree(args);
            let found = forbidden_crates_in_tree(&tree);
            assert!(
                found.is_empty(),
                "release cargo tree ({label}) contains {found:?}"
            );
        }
    }

    #[test]
    fn a_transitive_agpl_crate_is_visible_in_the_tree() {
        let tree = "\
thinwire v0.1.0
thinwire-protocol v0.1.0
whatsapp-rust v0.7.0
wacore v0.7.0
wacore-libsignal v0.1.0
libsignalx v0.1.0
";
        let found = forbidden_crates_in_tree(tree);
        assert_eq!(found, vec!["wacore-libsignal".to_string()]);
    }

    /// Package names from `cargo tree --prefix none`, including `├──` lines.
    fn forbidden_crates_in_tree(tree: &str) -> Vec<String> {
        let mut found = Vec::new();
        for line in tree.lines() {
            let Some(name) = package_name_on_tree_line(line) else {
                continue;
            };
            if is_forbidden_agpl_crate(name) && !found.iter().any(|seen| seen == name) {
                found.push(name.to_string());
            }
        }
        found
    }

    fn package_name_on_tree_line(line: &str) -> Option<&str> {
        let trimmed = line.trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
        let name = trimmed.split_whitespace().next()?;
        if name.is_empty() { None } else { Some(name) }
    }

    /// `libsignal-service` counts. `libsignalx` does not.
    fn is_forbidden_agpl_crate(name: &str) -> bool {
        for root in ["presage", "libsignal", "wacore-libsignal"] {
            if name == root
                || name
                    .strip_prefix(root)
                    .is_some_and(|rest| rest.starts_with('-'))
            {
                return true;
            }
        }
        false
    }

    fn release_cargo_tree(extra: &[&str]) -> String {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let output = std::process::Command::new(env!("CARGO"))
            .current_dir(root)
            .args(["tree", "-p", "thinwire", "--prefix", "none"])
            .args(extra)
            .env("CARGO_BUILD_JOBS", "4")
            .output()
            .expect("cargo tree");
        assert!(
            output.status.success(),
            "cargo tree failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("cargo tree utf-8")
    }

    #[test]
    fn agpl_clients_stay_behind_optional_features() {
        let protocol = include_str!("../Cargo.toml");
        let app = include_str!("../../thinwire/Cargo.toml");
        let core = include_str!("../../thinwire-core/Cargo.toml");
        let workspace = include_str!("../../../Cargo.toml");
        for manifest in [protocol, app, core, workspace] {
            assert!(
                !manifest_default_enables(manifest, "signal-local"),
                "signal-local must stay off the default feature set"
            );
            assert!(
                !manifest_default_enables(manifest, "whatsapp-web"),
                "whatsapp-web must stay off the default feature set"
            );
        }
        assert!(
            !protocol.contains("presage"),
            "thinwire-protocol must not depend on the AGPL Signal crate"
        );
        assert!(app.contains("thinwire-signal"));
        assert!(app.contains("signal-local"));
        assert!(core.contains("signal-local"));
        let signal = include_str!("../../thinwire-signal/Cargo.toml");
        assert!(signal.contains("AGPL-3.0-only"));
        assert!(signal.contains("presage"));
        let release = include_str!("../../../.github/workflows/os-zips.yml");
        assert!(release.contains("--features telegram-tdlib"));
        assert!(!release.contains("signal-local"));
        assert!(!release.contains("whatsapp-web"));
    }

    fn manifest_default_enables(manifest: &str, feature: &str) -> bool {
        manifest.lines().any(|line| {
            let trimmed = line.trim();
            trimmed.starts_with("default") && trimmed.contains(feature) && !trimmed.contains('#')
        })
    }

    fn contains_word(hay: &str, word: &str) -> bool {
        hay.split(|ch: char| !ch.is_ascii_alphabetic())
            .any(|token| token == word)
    }
}
