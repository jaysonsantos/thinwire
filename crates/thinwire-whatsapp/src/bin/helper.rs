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
//! text, phone numbers, QR data, or pair codes.

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
fn init_tracing() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
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
