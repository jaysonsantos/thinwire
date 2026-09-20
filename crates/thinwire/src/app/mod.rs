//! eframe application: polls adapter events and draws the shell.

mod auth;
mod snapshot;
mod ui;

use std::time::Duration;

use eframe::egui;
use thinwire_protocol::AdapterHost;

use snapshot::Snapshot;

/// Native thinwire window. Protocol work stays on the stored tokio runtime.
pub struct ThinwireApp {
    _runtime: tokio::runtime::Runtime,
    host: AdapterHost,
    snapshot: Snapshot,
}

impl ThinwireApp {
    #[must_use]
    pub fn new() -> Self {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime for protocol adapters");
        let host = AdapterHost::spawn(runtime.handle());
        Self {
            _runtime: runtime,
            host,
            snapshot: Snapshot::new(),
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

impl Default for ThinwireApp {
    fn default() -> Self {
        Self::new()
    }
}

impl eframe::App for ThinwireApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();
        // Poll the worker channel while idle so events do not wait on input.
        // logic() still runs when the window is hidden after a repaint request.
        ctx.request_repaint_after(Duration::from_millis(100));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui::draw(ui, &mut self.snapshot);
        self.flush_commands();
    }
}
