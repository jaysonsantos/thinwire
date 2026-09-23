//! eframe application: polls adapter events and draws the shell.

mod auth;
mod secrets;
mod settings;
mod snapshot;
mod ui;
#[cfg(feature = "whatsapp-web")]
mod whatsapp_gate;

use std::sync::Arc;
use std::time::Duration;

use eframe::egui;
use thinwire_protocol::{
    AdapterCommand, AdapterHost, DiscordAdapter, DiscordSecretVault, ProtocolId,
    TelegramSecretVault, WhatsAppPhoneVault,
};

use secrets::SecretStore;
use snapshot::Snapshot;

pub use settings::Settings;

/// Native thinwire window. Protocol and keychain work stay off the UI thread.
pub struct ThinwireApp {
    runtime: tokio::runtime::Runtime,
    host: AdapterHost,
    snapshot: Snapshot,
    settings: Settings,
    secrets: Arc<SecretStore>,
    whatsapp_phone: Arc<WhatsAppPhoneVault>,
    last_os_theme: Option<egui::Theme>,
}

impl ThinwireApp {
    #[must_use]
    pub fn new(settings: Settings) -> Self {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime for protocol adapters");
        let secrets = SecretStore::for_ui(runtime.handle());
        let whatsapp_phone = Arc::new(WhatsAppPhoneVault::new());
        let host = AdapterHost::spawn(
            runtime.handle(),
            Arc::clone(&secrets) as Arc<dyn TelegramSecretVault>,
            Arc::clone(&secrets) as Arc<dyn DiscordSecretVault>,
            Arc::clone(&whatsapp_phone),
        );
        // `for_ui` only schedules keychain attach. Discord's start can run
        // before that blocking read finishes, so arm it again once a token
        // is in memory. The hook sends a command; it does not touch the OS store.
        bind_discord_after_hydrate(&secrets, &host);
        let mut snapshot = Snapshot::new();
        snapshot.status_text = secret_store_status_text(secrets.backend_name());
        Self {
            runtime,
            host,
            snapshot,
            settings,
            secrets,
            whatsapp_phone,
            last_os_theme: None,
        }
    }

    fn drain_events(&mut self) {
        for event in self.host.poll_events() {
            self.snapshot.apply(event);
        }
    }

    fn refresh_secret_store_status(&mut self) {
        if let Some(text) =
            refreshed_secret_store_status(&self.snapshot.status_text, self.secrets.backend_name())
        {
            self.snapshot.status_text = text;
        }
    }

    fn flush_commands(&mut self) {
        for command in self.snapshot.take_commands() {
            self.host.send(command);
        }
        if self.snapshot.take_keychain_flush() {
            self.secrets.spawn_os_flush(self.runtime.handle());
        }
        if let Some(job) = self.settings.take_persist_job() {
            self.runtime.handle().spawn_blocking(move || job.run());
        }
    }
}

impl eframe::App for ThinwireApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();
        self.refresh_secret_store_status();
        self.snapshot.poll_resume(&self.secrets);
        if self.settings.follow_os_live(ctx, &mut self.last_os_theme) {
            ctx.request_repaint();
        }
        // Poll the worker channel while idle so events do not wait on input.
        // logic() still runs when the window is hidden after a repaint request.
        ctx.request_repaint_after(Duration::from_millis(100));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui::draw(
            ui,
            &mut self.snapshot,
            &mut self.settings,
            &self.secrets,
            &self.whatsapp_phone,
        );
        self.flush_commands();
    }
}

fn bind_discord_after_hydrate(secrets: &SecretStore, host: &AdapterHost) {
    if !DiscordAdapter::bot_inbox_compiled() {
        return;
    }
    let commands = host.command_sender();
    secrets.on_discord_token_hydrated(move || {
        let _ = commands.send(AdapterCommand::Connect {
            protocol: ProtocolId::Discord,
        });
    });
}

const SECRET_STORE_STATUS_PREFIX: &str = "Sign in with Telegram to get started.";

fn secret_store_status_text(backend: &str) -> String {
    let _ = backend;
    SECRET_STORE_STATUS_PREFIX.to_string()
}

fn refreshed_secret_store_status(current: &str, backend: &str) -> Option<String> {
    if current != SECRET_STORE_STATUS_PREFIX {
        return None;
    }
    let next = secret_store_status_text(backend);
    (current != next).then_some(next)
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use thinwire_protocol::{AdapterEvent, AdapterStatus, DiscordSecretVault, TelegramSecretVault};
    use tokio::runtime::Handle;

    use super::*;

    #[tokio::test]
    async fn discord_arms_after_keychain_hydrate() {
        if !DiscordAdapter::bot_inbox_compiled() {
            return;
        }
        let store = SecretStore::detached_for_test();
        let whatsapp_phone = Arc::new(WhatsAppPhoneVault::new());
        let mut host = AdapterHost::spawn(
            &Handle::current(),
            Arc::clone(&store) as Arc<dyn TelegramSecretVault>,
            Arc::clone(&store) as Arc<dyn DiscordSecretVault>,
            Arc::clone(&whatsapp_phone),
        );
        bind_discord_after_hydrate(&store, &host);

        let deadline = Instant::now() + Duration::from_secs(3);
        let mut saw_missing = false;
        while !saw_missing {
            for event in host.poll_events() {
                if let AdapterEvent::Status { detail, .. } = &event {
                    assert!(!detail.contains("fixture-bot-token"));
                    if detail.contains("bot token is not in the OS keychain") {
                        saw_missing = true;
                    }
                }
            }
            if saw_missing {
                break;
            }
            if Instant::now() > deadline {
                panic!("discord started without reporting a missing token");
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        store.complete_discord_hydrate_for_test(Some("fixture-bot-token"));

        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            for event in host.poll_events() {
                if let AdapterEvent::Status { status, detail, .. } = &event {
                    assert!(!detail.contains("fixture-bot-token"));
                    if *status == AdapterStatus::Stubbed
                        && detail.contains("bot token is in the OS keychain")
                        && !detail.contains("not in the OS keychain")
                    {
                        return;
                    }
                }
            }
            if Instant::now() > deadline {
                panic!("discord did not arm after keychain hydrate");
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[test]
    fn secret_store_status_stays_calm_across_backend_attach() {
        let current = secret_store_status_text("memory");
        assert_eq!(current, "Sign in with Telegram to get started.");
        assert_eq!(refreshed_secret_store_status(&current, "os-keychain"), None);
    }

    #[test]
    fn secret_store_status_does_not_clobber_auth_copy() {
        assert_eq!(
            refreshed_secret_store_status("Telegram: enter a phone number.", "os-keychain"),
            None
        );
    }
}
