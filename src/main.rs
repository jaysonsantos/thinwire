use eframe::egui;

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([960.0, 640.0])
            .with_title("thinwire"),
        ..Default::default()
    };

    eframe::run_native(
        "thinwire",
        options,
        Box::new(|_cc| Ok(Box::new(ThinwireApp::default()))),
    )
}

#[derive(Default)]
struct ThinwireApp;

impl eframe::App for ThinwireApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("thinwire");
            ui.label("Lightweight multi-protocol messenger shell.");
            ui.separator();
            ui.label("UI stays on the main thread. Protocol work must stay off it.");
        });
    }
}
