//! Protocol adapters, capability metadata, and the tokio host.

mod adapter;
#[cfg(test)]
mod contract;
pub mod demo;
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
    AccountState, AdapterCommand, AdapterError, AdapterEvent, AdapterStatus, Arrival, ChatMessage,
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
        let closure = dependency_closure(
            include_str!("../../../Cargo.lock"),
            "thinwire-protocol",
            &release_skipped_deps(protocol),
        );
        for name in [
            "presage",
            "libsignal",
            "libsignal-service",
            "wacore-libsignal",
        ] {
            assert!(
                !closure.iter().any(|pkg| pkg == name),
                "thinwire-protocol closure contains {name}"
            );
        }
    }

    #[test]
    fn a_lock_walk_finds_an_agpl_crate_two_levels_down() {
        let lock = r#"
[[package]]
name = "thinwire-protocol"
version = "0.1.0"
dependencies = [
    "middle 1.0.0",
]

[[package]]
name = "middle"
version = "1.0.0"
dependencies = [
    "wacore-libsignal 0.7.0",
]

[[package]]
name = "middle"
version = "2.0.0"
dependencies = [
    "not-selected",
]

[[package]]
name = "wacore-libsignal"
version = "0.7.0"
dependencies = [
]

[[package]]
name = "wacore-libsignal"
version = "9.9.9"
dependencies = [
    "other-version",
]

[[package]]
name = "not-selected"
version = "1.0.0"
dependencies = [
]

[[package]]
name = "other-version"
version = "1.0.0"
dependencies = [
]
"#;
        let closure = dependency_closure(lock, "thinwire-protocol", &[]);
        assert!(closure.contains("wacore-libsignal"));
        assert!(closure.contains("middle"));
        assert!(!closure.contains("not-selected"));
        assert!(!closure.contains("other-version"));
    }

    /// Every package reachable from `root` in `Cargo.lock`.
    /// A dependency line is `name` or `name version` (source text after the version is ignored).
    /// A line with no version follows every version of that name.
    /// Optional dependencies that `default` and `telegram-tdlib` do not enable.
    /// The lock still lists them. The release closure does not enter them.
    fn release_skipped_deps(manifest: &str) -> Vec<String> {
        let optional = optional_dep_names(manifest);
        let enabled = enabled_dep_names(manifest, &["default", "telegram-tdlib"]);
        optional
            .into_iter()
            .filter(|name| !enabled.iter().any(|on| on == name))
            .collect()
    }

    fn optional_dep_names(manifest: &str) -> Vec<String> {
        manifest
            .lines()
            .filter_map(|line| {
                let trimmed = line.trim();
                if !trimmed.contains("optional = true") {
                    return None;
                }
                let name = trimmed.split('=').next()?.trim();
                if name.is_empty() || name.starts_with('[') {
                    None
                } else {
                    Some(name.to_string())
                }
            })
            .collect()
    }

    fn enabled_dep_names(manifest: &str, features: &[&str]) -> Vec<String> {
        let mut names = Vec::new();
        for feature in features {
            let header = format!("{feature} =");
            let Some(start) = manifest.find(&header) else {
                continue;
            };
            let rest = &manifest[start + header.len()..];
            let body = if let Some(inline) = rest.trim_start().strip_prefix('[') {
                inline.split(']').next().unwrap_or("")
            } else {
                ""
            };
            for token in body.split([',', '"', '\n']) {
                let token = token.trim();
                if let Some(name) = token.strip_prefix("dep:") {
                    names.push(name.to_string());
                }
            }
        }
        names
    }

    fn dependency_closure(
        lock: &str,
        root: &str,
        skip_from_root: &[String],
    ) -> std::collections::BTreeSet<String> {
        let packages = lock_packages(lock);
        let mut by_name: std::collections::BTreeMap<&str, Vec<usize>> =
            std::collections::BTreeMap::new();
        for (index, package) in packages.iter().enumerate() {
            by_name.entry(package.name).or_default().push(index);
        }
        let mut seen_ids = std::collections::BTreeSet::new();
        let mut names = std::collections::BTreeSet::new();
        let mut stack: Vec<usize> = by_name.get(root).cloned().unwrap_or_default();
        while let Some(index) = stack.pop() {
            let id = (packages[index].name, packages[index].version);
            if !seen_ids.insert(id) {
                continue;
            }
            names.insert(packages[index].name.to_string());
            for dep in &packages[index].deps {
                if packages[index].name == root
                    && skip_from_root.iter().any(|name| name == dep.name)
                {
                    continue;
                }
                let Some(indexes) = by_name.get(dep.name) else {
                    continue;
                };
                for &dep_index in indexes {
                    if dep
                        .version
                        .is_none_or(|version| packages[dep_index].version == version)
                    {
                        stack.push(dep_index);
                    }
                }
            }
        }
        names
    }

    struct LockPackage<'a> {
        name: &'a str,
        version: &'a str,
        deps: Vec<LockDep<'a>>,
    }

    struct LockDep<'a> {
        name: &'a str,
        version: Option<&'a str>,
    }

    fn lock_packages(lock: &str) -> Vec<LockPackage<'_>> {
        lock.split("[[package]]")
            .skip(1)
            .filter_map(parse_lock_package)
            .collect()
    }

    fn parse_lock_package(block: &str) -> Option<LockPackage<'_>> {
        let name = field(block, "name")?;
        let version = field(block, "version").unwrap_or("");
        let deps = block
            .split("dependencies = [")
            .nth(1)
            .and_then(|rest| rest.split("]").next())
            .map(parse_dep_lines)
            .unwrap_or_default();
        Some(LockPackage {
            name,
            version,
            deps,
        })
    }

    fn field<'a>(block: &'a str, key: &str) -> Option<&'a str> {
        let prefix = format!("{key} = \"");
        let line = block
            .lines()
            .find(|line| line.trim().starts_with(&prefix))?;
        let start = line.find('"')? + 1;
        let end = line[start..].find('"')? + start;
        Some(&line[start..end])
    }

    fn parse_dep_lines(block: &str) -> Vec<LockDep<'_>> {
        block
            .lines()
            .filter_map(|line| {
                let quoted = line.trim().trim_matches(',').trim().strip_prefix('"')?;
                let spec = quoted.split('"').next()?.trim();
                if spec.is_empty() {
                    return None;
                }
                let mut parts = spec.split_whitespace();
                let name = parts.next()?;
                let version = parts.next();
                Some(LockDep { name, version })
            })
            .collect()
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
