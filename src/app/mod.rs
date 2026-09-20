//! eframe application: polls adapter events and draws the three-pane shell.

mod snapshot;
mod ui;

use std::time::Duration;

use eframe::egui;

use crate::protocols::AdapterHost;

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
}

impl Default for ThinwireApp {
    fn default() -> Self {
        Self::new()
    }
}

impl eframe::App for ThinwireApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();
        ui::draw(ctx, &mut self.snapshot);
        // Poll the channel while idle so worker events do not wait on input.
        ctx.request_repaint_after(Duration::from_millis(100));
    }
}
