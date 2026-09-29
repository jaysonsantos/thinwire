//! Protocol adapters, capability metadata, and the tokio host.

mod adapter;
#[cfg(any(test, feature = "contract-kit"))]
pub mod contract;
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
    APP_CLOSE_LIMIT, AccountState, AdapterCommand, AdapterError, AdapterEvent, AdapterStatus,
    Arrival, ChatMessage, Conversation, Delivery, DiscordAuthMode, EventTx, ProtocolAdapter,
    ProtocolCapabilities, ProtocolId, RedactedPairingSecret, SupportClass, TelegramAuthError,
    TelegramAuthPhase, TelegramAuthStep, TelegramCodeVia, emit_account, emit_chat_list_loaded,
    emit_conversation, emit_history_loaded, emit_message, emit_send_accepted, emit_send_rejected,
    emit_status, emit_stopped,
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
    SlackEventStream, SlackHistoryPage, SlackInbound, SlackInbox, SlackInstallGrant,
    SlackInstalledWorkspace, SlackLoopback, SlackPost, SlackSecretKey, SlackSecretVault,
    SlackWebApi, WORKSPACE_BOT_SCOPES, authorize_url, loopback_redirect_uri, new_oauth_state,
    parse_loopback_callback, resolve_slack_app_token, resolve_slack_client,
};
#[cfg(feature = "slack-oauth")]
pub use slack::{oauth_v2_access_request, socket_mode_config, workspace_bot_token};
pub use telegram::{
    TelegramAdapter, TelegramApiOrigin, TelegramApiSource, parse_telegram_chat_id,
    resolve_telegram_api, telegram_api_available,
};
pub use whatsapp::{WhatsAppAdapter, WhatsAppPhoneVault, parse_whatsapp_chat_id};

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
    replacements: Vec<Box<dyn ProtocolAdapter>>,
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
    // A local-only build puts its AGPL clients (Signal, WhatsApp) in place of
    // the MIT stubs. This crate does not depend on those crates (ADR 0011).
    for replacement in replacements {
        let id = replacement.id();
        if let Some(slot) = adapters.iter_mut().find(|adapter| adapter.id() == id) {
            *slot = replacement;
        }
    }
    adapters
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for a local-only AGPL client: only its id and caps matter.
    struct Replacement(ProtocolCapabilities);

    impl ProtocolAdapter for Replacement {
        fn id(&self) -> ProtocolId {
            self.0.id
        }
        fn capabilities(&self) -> ProtocolCapabilities {
            self.0
        }
        fn start(&mut self, _events: EventTx) {}
        fn handle(
            &mut self,
            _command: AdapterCommand,
            _events: &EventTx,
        ) -> Result<(), AdapterError> {
            Ok(())
        }
    }

    /// #77: each replacement takes the slot of the stub with its protocol
    /// id. The order and the other adapters stay.
    #[test]
    fn registry_puts_each_replacement_in_its_protocol_slot() {
        let replaced = |id: ProtocolId, detail: &'static str| {
            let caps = catalog()
                .into_iter()
                .find(|caps| caps.id == id)
                .expect("in the catalog");
            Box::new(Replacement(ProtocolCapabilities { detail, ..caps }))
                as Box<dyn ProtocolAdapter>
        };
        let adapters = registry(
            std::sync::Arc::new(MemorySecretVault::new()),
            std::sync::Arc::new(MemoryDiscordVault::new()),
            std::sync::Arc::new(MemorySlackVault::new()),
            std::sync::Arc::new(WhatsAppPhoneVault::new()),
            std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            vec![
                replaced(ProtocolId::Signal, "signal replacement"),
                replaced(ProtocolId::WhatsApp, "whatsapp replacement"),
            ],
        );
        let ids: Vec<ProtocolId> = adapters.iter().map(|adapter| adapter.id()).collect();
        let catalog_ids: Vec<ProtocolId> = catalog().iter().map(|caps| caps.id).collect();
        assert_eq!(ids, catalog_ids, "one adapter per protocol, catalog order");
        let detail = |id: ProtocolId| {
            adapters
                .iter()
                .find(|adapter| adapter.id() == id)
                .expect("adapter")
                .capabilities()
                .detail
        };
        assert_eq!(detail(ProtocolId::WhatsApp), "whatsapp replacement");
        assert_eq!(detail(ProtocolId::Signal), "signal replacement");
        assert_ne!(detail(ProtocolId::Telegram), "whatsapp replacement");
    }

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
        let build = parse_release_build(release).expect("release command");
        assert_eq!(build.package, "thinwire");
        assert_eq!(build.features, vec!["telegram-tdlib".to_string()]);
        assert!(
            build.targets.len() >= 3,
            "each OS zip is covered by the lock walk"
        );
        assert!(build.targets.iter().any(|os| os.contains("ubuntu")));
        assert!(build.targets.iter().any(|os| os.contains("macos")));
        assert!(build.targets.iter().any(|os| os.contains("windows")));
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
        let core = include_str!("../../thinwire-core/Cargo.toml");
        let closure = dependency_closure(
            include_str!("../../../Cargo.lock"),
            &build.package,
            &release_skipped_deps(&[app, core, protocol], &build.features),
        );
        for name in [
            "presage",
            "libsignal",
            "libsignal-service",
            "wacore-libsignal",
        ] {
            assert!(
                !closure.iter().any(|pkg| pkg == name),
                "release closure contains {name}"
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

    #[test]
    fn a_lock_walk_from_the_release_package_keeps_a_target_specific_edge() {
        let lock = r#"
[[package]]
name = "thinwire"
version = "0.1.0"
dependencies = [
    "winapi 0.3.9",
]

[[package]]
name = "winapi"
version = "0.3.9"
dependencies = [
    "wacore-libsignal 0.7.0",
]

[[package]]
name = "wacore-libsignal"
version = "0.7.0"
dependencies = [
]
"#;
        let closure = dependency_closure(lock, "thinwire", &[]);
        assert!(closure.contains("wacore-libsignal"));
        assert!(closure.contains("winapi"));
    }

    #[test]
    fn a_disabled_optional_is_skipped_only_on_its_declaring_edge() {
        let thinwire = r#"
[package]
name = "thinwire"

[features]
default = []
telegram-tdlib = ["thinwire-protocol/telegram-tdlib"]
"#;
        let protocol = r#"
[package]
name = "thinwire-protocol"

[features]
default = []
telegram-tdlib = ["dep:tdlib-rs"]
discord-bot = ["dep:rustls"]

[dependencies]
tdlib-rs = { version = "1.4", optional = true }
rustls = { version = "0.23", optional = true }
secret-optional = { version = "1", optional = true }
"#;
        let skipped = release_skipped_deps(&[thinwire, protocol], &["telegram-tdlib".to_string()]);
        assert!(
            skipped
                .iter()
                .any(|(package, dep)| package == "thinwire-protocol" && dep == "rustls")
        );
        assert!(
            skipped
                .iter()
                .any(|(package, dep)| package == "thinwire-protocol" && dep == "secret-optional")
        );
        assert!(
            !skipped.iter().any(|(_, dep)| dep == "tdlib-rs"),
            "an enabled optional stays in the closure"
        );
        let lock = r#"
[[package]]
name = "thinwire"
version = "0.1.0"
dependencies = [
    "thinwire-protocol 0.1.0",
    "tdlib-rs 1.4.0",
]

[[package]]
name = "thinwire-protocol"
version = "0.1.0"
dependencies = [
    "rustls 0.23.0",
    "secret-optional 1.0.0",
    "tdlib-rs 1.4.0",
]

[[package]]
name = "tdlib-rs"
version = "1.4.0"
dependencies = [
    "ureq 3.0.0",
]

[[package]]
name = "ureq"
version = "3.0.0"
dependencies = [
    "rustls 0.23.0",
]

[[package]]
name = "rustls"
version = "0.23.0"
dependencies = [
    "wacore-libsignal 0.7.0",
]

[[package]]
name = "secret-optional"
version = "1.0.0"
dependencies = [
    "presage 0.8.0",
]

[[package]]
name = "wacore-libsignal"
version = "0.7.0"
dependencies = [
]

[[package]]
name = "presage"
version = "0.8.0"
dependencies = [
]
"#;
        let closure = dependency_closure(lock, "thinwire", &skipped);
        assert!(closure.contains("tdlib-rs"));
        assert!(closure.contains("ureq"));
        assert!(
            closure.contains("rustls"),
            "ureq -> rustls stays when rustls is also a disabled optional of thinwire-protocol"
        );
        assert!(
            closure.contains("wacore-libsignal"),
            "a crate under the enabled rustls edge stays in the release closure"
        );
        assert!(
            !closure.contains("secret-optional"),
            "the disabled optional edge from its declaring package is skipped"
        );
        assert!(
            !closure.contains("presage"),
            "a crate reached only through that disabled edge stays out"
        );
    }

    #[test]
    fn a_release_command_rejects_all_features() {
        let workflow = "run: cargo build --release -p thinwire --all-features\n";
        assert!(parse_release_build(workflow).is_err());
    }

    struct ReleaseBuild {
        package: String,
        features: Vec<String>,
        targets: Vec<String>,
    }

    /// The `cargo build` line in `os-zips.yml`, plus every matrix OS.
    /// `--all-features` is refused: it would enable `signal-local` and `whatsapp-web`.
    fn parse_release_build(workflow: &str) -> Result<ReleaseBuild, &'static str> {
        let line = workflow
            .lines()
            .find(|line| line.contains("cargo build"))
            .ok_or("release workflow has no cargo build")?;
        if line.contains("--all-features") {
            return Err("release command enables every feature");
        }
        let package = flag_value(line, "-p")
            .or_else(|| flag_value(line, "--package"))
            .ok_or("release command names no package")?
            .to_string();
        let features = flag_value(line, "--features")
            .unwrap_or("")
            .split([',', ' '])
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .map(str::to_string)
            .collect();
        let targets = workflow
            .lines()
            .filter_map(|line| {
                let trimmed = line.trim().trim_start_matches('-').trim();
                let os = trimmed.strip_prefix("os:")?.trim().trim_matches('"');
                (!os.is_empty()).then(|| os.to_string())
            })
            .collect();
        Ok(ReleaseBuild {
            package,
            features,
            targets,
        })
    }

    fn flag_value<'a>(line: &'a str, flag: &str) -> Option<&'a str> {
        let mut parts = line.split_whitespace();
        while let Some(part) = parts.next() {
            if part == flag {
                return parts.next();
            }
            if let Some(value) = part.strip_prefix(&format!("{flag}=")) {
                return Some(value);
            }
        }
        None
    }

    /// Optional dependencies the release features leave off.
    /// Each entry is the crate that declares the optional and the dependency
    /// name. The walk skips that edge only. A later package that depends on
    /// the same name stays in the closure. The walk has no host target, so a
    /// macOS-only or Windows-only edge stays.
    fn release_skipped_deps(manifests: &[&str], features: &[String]) -> Vec<(String, String)> {
        let crates = manifests
            .iter()
            .filter_map(|manifest| parse_crate(manifest))
            .collect::<Vec<_>>();
        let enabled = enabled_optional_deps(&crates, "thinwire", features);
        let mut skipped = Vec::new();
        for krate in &crates {
            for name in &krate.optional {
                if enabled.iter().any(|on| on == name) {
                    continue;
                }
                let edge = (krate.name.clone(), name.clone());
                if !skipped.iter().any(|on| on == &edge) {
                    skipped.push(edge);
                }
            }
        }
        skipped
    }

    struct CrateFeatures {
        name: String,
        optional: Vec<String>,
        features: std::collections::BTreeMap<String, Vec<String>>,
    }

    fn parse_crate(manifest: &str) -> Option<CrateFeatures> {
        let name = manifest.lines().find_map(|line| {
            let trimmed = line.trim();
            let rest = trimmed.strip_prefix("name = \"")?;
            Some(rest.split('"').next()?.to_string())
        })?;
        Some(CrateFeatures {
            name,
            optional: optional_dep_names(manifest),
            features: feature_table(manifest),
        })
    }

    fn feature_table(manifest: &str) -> std::collections::BTreeMap<String, Vec<String>> {
        let mut features = std::collections::BTreeMap::new();
        let Some(section) = manifest.split("[features]").nth(1) else {
            return features;
        };
        let body = section.split("\n[").next().unwrap_or(section);
        let mut lines = body.lines().peekable();
        while let Some(line) = lines.next() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let Some((name, rest)) = trimmed.split_once('=') else {
                continue;
            };
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            let rest = rest.trim();
            let tokens = if let Some(inline) = rest.strip_prefix('[') {
                let mut text = inline.to_string();
                if !inline.contains(']') {
                    for next in lines.by_ref() {
                        text.push('\n');
                        text.push_str(next);
                        if next.contains(']') {
                            break;
                        }
                    }
                }
                feature_tokens(text.split(']').next().unwrap_or(""))
            } else {
                feature_tokens(rest)
            };
            features.insert(name.to_string(), tokens);
        }
        features
    }

    fn feature_tokens(body: &str) -> Vec<String> {
        body.split([',', '"', '\n', ' '])
            .map(str::trim)
            .filter(|token| !token.is_empty() && *token != "[" && *token != "]")
            .map(str::to_string)
            .collect()
    }

    fn enabled_optional_deps(
        crates: &[CrateFeatures],
        root: &str,
        features: &[String],
    ) -> Vec<String> {
        let mut queue = Vec::new();
        for feature in features {
            queue.push((root.to_string(), feature.clone()));
        }
        queue.push((root.to_string(), "default".to_string()));
        let mut seen = std::collections::BTreeSet::new();
        let mut deps = Vec::new();
        while let Some((package, feature)) = queue.pop() {
            if !seen.insert((package.clone(), feature.clone())) {
                continue;
            }
            let Some(krate) = crates.iter().find(|krate| krate.name == package) else {
                continue;
            };
            for token in krate.features.get(&feature).into_iter().flatten() {
                if let Some(dep) = token.strip_prefix("dep:") {
                    if !deps.iter().any(|name| name == dep) {
                        deps.push(dep.to_string());
                    }
                } else if let Some((dep_pkg, dep_feat)) = token.split_once('/') {
                    queue.push((dep_pkg.to_string(), dep_feat.to_string()));
                } else {
                    queue.push((package.clone(), token.clone()));
                }
            }
        }
        deps
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

    fn dependency_closure(
        lock: &str,
        root: &str,
        skip_edges: &[(String, String)],
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
                let from = packages[index].name;
                if skip_edges
                    .iter()
                    .any(|(package, name)| package == from && name == dep.name)
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
