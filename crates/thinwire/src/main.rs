mod app;

use eframe::egui;
use tracing_subscriber::prelude::*;

fn main() -> eframe::Result<()> {
    init_tracing();

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

/// `slack_morphism` logs the one-time Socket Mode URL. A more specific
/// `RUST_LOG` directive wins over `slack_morphism=off` on an `EnvFilter`, so
/// this layer drops that target on its own.
fn init_tracing() {
    use tracing_subscriber::filter::FilterExt;
    let layer = tracing_subscriber::fmt::layer()
        .with_filter(tracing_subscriber::EnvFilter::from_default_env().and(slack_log_filter()));
    tracing_subscriber::registry().with(layer).init();
}

fn slack_log_filter()
-> tracing_subscriber::filter::FilterFn<impl Fn(&tracing::Metadata<'_>) -> bool> {
    tracing_subscriber::filter::filter_fn(|metadata: &tracing::Metadata<'_>| {
        !metadata.target().starts_with("slack_morphism")
    })
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::prelude::*;

    use super::slack_log_filter;

    #[derive(Clone)]
    struct Buf(Arc<Mutex<Vec<u8>>>);

    impl Write for Buf {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("log buffer").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_specific_rust_log_does_not_log_slack_morphism() {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let writer = Buf(Arc::clone(&bytes));
        use tracing_subscriber::filter::FilterExt;
        let layer = tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .with_filter(
                tracing_subscriber::EnvFilter::new("info,slack_morphism::socket_mode=debug")
                    .and(slack_log_filter()),
            );
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::debug!(target: "slack_morphism::socket_mode", "wss://one-time.example");
            tracing::info!(target: "thinwire", "shell is up");
        });
        let output = String::from_utf8(bytes.lock().expect("log buffer").clone()).expect("utf8");
        assert!(!output.contains("wss://"), "{output}");
        assert!(output.contains("shell is up"), "{output}");
    }
}
