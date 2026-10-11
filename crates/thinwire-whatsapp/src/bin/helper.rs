// SPDX-License-Identifier: AGPL-3.0-only
//! `thinwire-whatsapp-helper`: the WhatsApp client as its own program
//! (ADR 0013).
//!
//! The MIT thinwire app starts this program as a child process. The two
//! programs talk only through the wire protocol of `thinwire-ipc`: one JSON
//! object per line on this program's stdin and stdout. Logs go to stderr.
//! Nothing else writes to stdout.
//!
//! The program ends when the app sends `Shutdown`, when the app closes the
//! pipe, or on a line that is not in the protocol. It never logs message
//! text, phone numbers, QR data, or pair codes: it writes the log lines of
//! thinwire code only, because whatsapp-rust logs the pair code.

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use thinwire_protocol::WhatsAppPhoneVault;
use thinwire_protocol::helper::{HelperRefusal, ServeConfig, ServeEnd, refuse, serve};
use thinwire_whatsapp::{SessionLockError, WhatsAppAdapter, lock_session, set_session_dir};

/// Names of the environment variables of this program.
mod env {
    pub const SESSION_DIR: &str = "THINWIRE_WHATSAPP_SESSION_DIR";
    pub const ALLOW_NO_CLIENT: &str = "THINWIRE_WHATSAPP_ALLOW_NO_CLIENT";
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
/// script can refuse a helper that was built without `whatsapp-web`.
#[cfg(feature = "whatsapp-web")]
const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (whatsapp-web)");
#[cfg(not(feature = "whatsapp-web"))]
const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (no client)");

/// WhatsApp helper process of thinwire (AGPL-3.0-only). The thinwire app
/// starts it. It reads commands from stdin and writes events to stdout.
#[derive(Parser, Debug)]
#[command(name = "thinwire-whatsapp-helper", version = VERSION)]
struct Cli {
    /// Keep the session store in this folder, not under the app-data folder.
    #[arg(long, env = env::SESSION_DIR)]
    session_dir: Option<PathBuf>,
    /// Run a build that has no WhatsApp client. Only for the process tests:
    /// such a build cannot pair, so without this flag it refuses to run.
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

/// Only the log lines of thinwire code. whatsapp-rust logs "Pair code
/// generated: ..." at level `info`, and the app logs each stderr line of
/// this program again. A more specific `RUST_LOG` directive wins on an
/// `EnvFilter`, so this filter drops the other targets on its own.
fn own_targets() -> tracing_subscriber::filter::FilterFn<impl Fn(&tracing::Metadata<'_>) -> bool> {
    tracing_subscriber::filter::filter_fn(|metadata: &tracing::Metadata<'_>| {
        thinwire_protocol::helper::helper_log_target(metadata.target())
    })
}

async fn run(cli: Cli) -> i32 {
    let mut stdout = tokio::io::stdout();
    // A build with no client must not say that it carries WhatsApp: the app
    // would show the helper as running, and every pairing would fail.
    if !cfg!(feature = "whatsapp-web") && !cli.allow_no_client {
        tracing::error!("this helper was built without whatsapp-web: it has no client");
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
            tracing::error!(%error, "the WhatsApp helper did not start");
            let reason = match error {
                SessionLockError::InUse => HelperRefusal::SessionInUse,
                SessionLockError::NoDataDir | SessionLockError::Io(_) => HelperRefusal::NoDataDir,
            };
            refuse(&mut stdout, reason).await;
            return exit::REFUSED;
        }
    };
    let phone = Arc::new(WhatsAppPhoneVault::new());
    let adapter = WhatsAppAdapter::new(Arc::clone(&phone));
    let config = ServeConfig {
        helper_version: env!("CARGO_PKG_VERSION"),
        phone: Some(phone),
    };
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
        let env = tracing_subscriber::EnvFilter::new("trace,whatsapp_rust=trace");
        let fmt = tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(move || writer.clone());
        let emit = || {
            // whatsapp-rust uses the `log` crate, with this target and text.
            log::info!(target: "Bot/PairCode", "Pair code generated: ABCD-1234");
            log::debug!(target: "whatsapp_rust::pair", "qr ref 2@do-not-log");
            tracing::trace!(target: "wacore::libsignal", "frame do-not-log");
            tracing::info!(target: "thinwire_whatsapp::whatsapp::link", "the link worker is up");
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

    /// whatsapp-rust logs the pair code at level `info`, with the target
    /// "Bot/PairCode". The app logs each stderr line of the helper again, so
    /// that line must never reach stderr, also when `RUST_LOG` asks for
    /// every level. The log lines of thinwire code stay.
    #[test]
    fn the_helper_does_not_log_the_pair_code_of_the_library() {
        // With `RUST_LOG` only, the pair code is in the log: the test sends
        // the record on the path of the library.
        let open = logged(false);
        assert!(open.contains("ABCD-1234"), "{open}");

        let output = logged(true);
        assert!(!output.contains("ABCD-1234"), "{output}");
        assert!(!output.contains("do-not-log"), "{output}");
        assert!(output.contains("the link worker is up"), "{output}");
    }
}
