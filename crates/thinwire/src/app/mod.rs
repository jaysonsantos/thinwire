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
    AdapterHost, DiscordSecretVault, TelegramSecretVault, WhatsAppPhoneVault,
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

const SECRET_STORE_STATUS_PREFIX: &str = "Adapters are stubs. Secret store: ";

fn secret_store_status_text(backend: &str) -> String {
    format!("{SECRET_STORE_STATUS_PREFIX}{backend}.")
}

fn refreshed_secret_store_status(current: &str, backend: &str) -> Option<String> {
    if !current.starts_with(SECRET_STORE_STATUS_PREFIX) {
        return None;
    }
    let next = secret_store_status_text(backend);
    (current != next).then_some(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_store_status_updates_from_memory_to_os() {
        let current = secret_store_status_text("memory");
        assert_eq!(
            refreshed_secret_store_status(&current, "os-keychain").as_deref(),
            Some("Adapters are stubs. Secret store: os-keychain.")
        );
    }

    #[test]
    fn secret_store_status_does_not_clobber_auth_copy() {
        assert_eq!(
            refreshed_secret_store_status("Telegram: enter a phone number.", "os-keychain"),
            None
        );
    }
}
