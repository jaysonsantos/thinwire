//! eframe application: polls adapter events and draws the shell.

mod auth;
mod secrets;
mod settings;
mod snapshot;
mod ui;

use std::time::Duration;

use eframe::egui;
use thinwire_protocol::AdapterHost;

use secrets::SecretStore;
use snapshot::Snapshot;

pub use settings::Settings;

/// Native thinwire window. Protocol work stays on the stored tokio runtime.
pub struct ThinwireApp {
    _runtime: tokio::runtime::Runtime,
    host: AdapterHost,
    snapshot: Snapshot,
    settings: Settings,
    secrets: SecretStore,
}

impl ThinwireApp {
    #[must_use]
    pub fn new(settings: Settings) -> Self {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime for protocol adapters");
        let host = AdapterHost::spawn(runtime.handle());
        let secrets = SecretStore::open();
        let mut snapshot = Snapshot::new();
        snapshot.status_text = format!(
            "Adapters are stubs. Secret store: {}.",
            secrets.backend_name()
        );
        Self {
            _runtime: runtime,
            host,
            snapshot,
            settings,
            secrets,
        }
    }

    fn drain_events(&mut self) {
        for event in self.host.poll_events() {
            self.snapshot.apply(event);
        }
    }

    fn flush_commands(&mut self) {
        for command in self.snapshot.take_commands() {
            self.host.send(command);
        }
    }
}

impl eframe::App for ThinwireApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();
        self.settings.apply(ctx);
        // Poll the worker channel while idle so events do not wait on input.
        // logic() still runs when the window is hidden after a repaint request.
        ctx.request_repaint_after(Duration::from_millis(100));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui::draw(ui, &mut self.snapshot, &mut self.settings, &self.secrets);
        self.flush_commands();
    }
}
