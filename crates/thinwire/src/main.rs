mod app;

use eframe::egui;

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            // `slack_morphism` logs the one-time Socket Mode URL. Keep it off
            // even when RUST_LOG asks for more.
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("slack_morphism=off".parse().expect("static directive")),
        )
        .init();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1120.0, 720.0])
            .with_title("thinwire"),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };

    eframe::run_native(
        "thinwire",
        options,
        Box::new(|cc| {
            app::install_theme(&cc.egui_ctx);
            let settings = app::Settings::load();
            // First paint follows the stored mode. Missing file → System (ADR 0005).
            app::apply_theme(&cc.egui_ctx, settings.theme());
            Ok(Box::new(app::ThinwireApp::new(settings, &cc.egui_ctx)))
        }),
    )
}
