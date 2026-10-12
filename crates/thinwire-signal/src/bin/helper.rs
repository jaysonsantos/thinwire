// SPDX-License-Identifier: AGPL-3.0-only
//! `thinwire-signal-helper`: the Signal client as its own program
//! (ADR 0013, #248).
//!
//! The MIT thinwire app starts this program as a child process. The two
//! programs talk only through the wire protocol of `thinwire-ipc`: one JSON
//! object per line on this program's stdin and stdout. Logs go to stderr.
//! Nothing else writes to stdout.
//!
//! The program ends when the app sends `Shutdown`, when the app closes the
//! pipe, or on a line that is not in the protocol. It never logs message
//! text or QR data: it writes the log lines of thinwire code only, because
//! presage logs the provisioning URL.

use std::path::PathBuf;

use clap::Parser;
use thinwire_protocol::helper::{HelperRefusal, ServeConfig, ServeEnd, refuse, serve};
use thinwire_signal::{SessionLockError, SignalAdapter, lock_session, set_session_dir};

/// Names of the environment variables of this program.
mod env {
    pub const SESSION_DIR: &str = "THINWIRE_SIGNAL_SESSION_DIR";
    pub const ALLOW_NO_CLIENT: &str = "THINWIRE_SIGNAL_ALLOW_NO_CLIENT";
}

/// Exit codes of this program.
mod exit {
    /// The app ended the session.
    pub const OK: i32 = 0;
    /// The helper did not start: no session folder, or it is in use.
    pub const REFUSED: i32 = 3;
    /// The app sent a line that is not in the protocol.
    pub const BAD_LINE: i32 = 4;
    /// The tokio runtime did not start.
    pub const NO_RUNTIME: i32 = 5;
}

/// The version text. It says if this build has a client, so the release
/// script can refuse a helper that was built without `signal-local`.
#[cfg(feature = "signal-local")]
const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (signal-local)");
#[cfg(not(feature = "signal-local"))]
const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (no client)");

/// Signal helper process of thinwire (AGPL-3.0-only). The thinwire app
/// starts it. It reads commands from stdin and writes events to stdout.
#[derive(Parser, Debug)]
#[command(name = "thinwire-signal-helper", version = VERSION)]
struct Cli {
    /// Keep the session store in this folder, not under the app-data folder.
    #[arg(long, env = env::SESSION_DIR)]
    session_dir: Option<PathBuf>,
    /// Run a build that has no Signal client. Only for the process tests:
    /// such a build cannot link, so without this flag it refuses to run.
    #[arg(long, env = env::ALLOW_NO_CLIENT)]
    allow_no_client: bool,
}

fn main() {
    let cli = Cli::parse();
    init_tracing();
    let code = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime.block_on(run(cli)),
        Err(error) => {
            tracing::error!(%error, "the tokio runtime did not start");
            exit::NO_RUNTIME
        }
    };
    // `exit`, not a return: a read of stdin that still waits on its thread
    // must not keep the process alive.
    std::process::exit(code);
}

/// Logs go to stderr only: stdout carries the wire protocol.
///
/// `RUST_LOG` sets the level of thinwire targets. It cannot turn on the log
/// of a library: see [`own_targets`].
fn init_tracing() {
    use tracing_subscriber::filter::FilterExt;
    use tracing_subscriber::prelude::*;
    let layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_filter(tracing_subscriber::EnvFilter::from_default_env().and(own_targets()));
    tracing_subscriber::registry().with(layer).init();
}

/// Only the log lines of thinwire code. presage logs "generating qrcode
/// from provisioning link: ..." at level `info`, and the app logs each
/// stderr line of this program again. A more specific `RUST_LOG` directive
/// wins on an `EnvFilter`, so this filter drops the other targets on its
/// own. The app had the same rule while the client ran in the app.
fn own_targets() -> tracing_subscriber::filter::FilterFn<impl Fn(&tracing::Metadata<'_>) -> bool> {
    tracing_subscriber::filter::filter_fn(|metadata: &tracing::Metadata<'_>| {
        thinwire_protocol::helper::helper_log_target(metadata.target())
    })
}

async fn run(cli: Cli) -> i32 {
    let mut stdout = tokio::io::stdout();
    // A build with no client must not say that it carries Signal: the app
    // would show the helper as running, and every link would fail.
    if !cfg!(feature = "signal-local") && !cli.allow_no_client {
        tracing::error!("this helper was built without signal-local: it has no client");
        refuse(&mut stdout, HelperRefusal::NoClient).await;
        return exit::REFUSED;
    }
    if let Some(dir) = cli.session_dir {
        // The first and only call: `main` runs once.
        let _ = set_session_dir(dir);
    }
    // Held until the process ends: one helper for each session store.
    let _lock = match lock_session() {
        Ok(lock) => lock,
        Err(error) => {
            tracing::error!(%error, "the Signal helper did not start");
            let reason = match error {
                SessionLockError::InUse => HelperRefusal::SessionInUse,
                SessionLockError::NoDataDir | SessionLockError::Io(_) => HelperRefusal::NoDataDir,
            };
            refuse(&mut stdout, reason).await;
            return exit::REFUSED;
        }
    };
    let config = ServeConfig {
        helper_version: env!("CARGO_PKG_VERSION"),
        // Signal links with a QR only: no phone number crosses the pipe.
        phone: None,
    };
    let adapter = SignalAdapter::new();
    match serve(Box::new(adapter), config, tokio::io::stdin(), stdout).await {
        ServeEnd::Shutdown | ServeEnd::AppGone => exit::OK,
        ServeEnd::BadLine(error) => {
            tracing::error!(%error, "the app sent a line that is not in the protocol");
            exit::BAD_LINE
        }
        ServeEnd::NotAHelperProtocol => exit::REFUSED,
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::filter::FilterExt;
    use tracing_subscriber::prelude::*;

    use super::own_targets;

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

    /// What the helper writes for these log lines, with and without the
    /// filter of the helper. `RUST_LOG` asks for every level.
    fn logged(own_only: bool) -> String {
        // The bridge that `init_tracing` installs for `log` records. It is
        // set one time for the process: a second call fails, and that is
        // fine.
        let _ = tracing_log::LogTracer::init();
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let writer = Buf(Arc::clone(&bytes));
        let env = tracing_subscriber::EnvFilter::new("trace,presage::manager=trace");
        let fmt = tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(move || writer.clone());
        let emit = || {
            // presage logs this line, with this target, at level `info`.
            tracing::info!(
                target: "presage::manager::linking",
                "generating qrcode from provisioning link: sgnl://linkdevice?uuid=ABCD-1234"
            );
            tracing::debug!(target: "libsignal_service::provisioning", "cipher do-not-log");
            log::trace!(target: "reqwest::connect", "authorization do-not-log");
            tracing::info!(target: "thinwire_signal::signal::owner", "the link owner is up");
        };
        if own_only {
            let subscriber =
                tracing_subscriber::registry().with(fmt.with_filter(env.and(own_targets())));
            tracing::subscriber::with_default(subscriber, emit);
        } else {
            let subscriber = tracing_subscriber::registry().with(fmt.with_filter(env));
            tracing::subscriber::with_default(subscriber, emit);
        }
        String::from_utf8(bytes.lock().expect("log buffer").clone()).expect("utf8")
    }

    /// PR #281 review: presage logs the provisioning URL at level `info`.
    /// The app logs each stderr line of the helper again, so that line must
    /// never reach stderr, also when `RUST_LOG` asks for every level. The
    /// log lines of thinwire code stay.
    #[test]
    fn the_helper_does_not_log_the_provisioning_url_of_the_library() {
        // With `RUST_LOG` only, the URL is in the log: the test sends the
        // line on the path of the library.
        let open = logged(false);
        assert!(open.contains("sgnl://linkdevice"), "{open}");

        let output = logged(true);
        assert!(!output.contains("sgnl://"), "{output}");
        assert!(!output.contains("ABCD-1234"), "{output}");
        assert!(!output.contains("do-not-log"), "{output}");
        assert!(output.contains("the link owner is up"), "{output}");
    }
}
