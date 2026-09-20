use eframe::egui;

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1120.0, 720.0])
            .with_title("thinwire"),
        ..Default::default()
    };

    eframe::run_native(
        "thinwire",
        options,
        Box::new(|_cc| Ok(Box::new(thinwire::app::ThinwireApp::new()))),
    )
}
