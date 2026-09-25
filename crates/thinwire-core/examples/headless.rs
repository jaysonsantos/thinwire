//! Headless frontend: drives `thinwire-core` with no GUI toolkit (ADR 0010).
//!
//! Run: `cargo run -p thinwire-core --example headless`
//!
//! It keeps secrets in memory, prints the account status lines while the
//! adapters start, then shuts the clients down. It prints no secret and no
//! adapter detail text.
//!
//! Threads: every `Core` call runs on the `main` thread, outside
//! `block_on`. `block_on` waits only for the change signal (#55).

use std::time::{Duration, Instant};

use thinwire_core::{Core, CoreConfig};

/// How long the example watches the adapters before it shuts down.
const RUN_FOR: Duration = Duration::from_secs(2);
/// Longest wait for the clients to close.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

fn main() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let mut core = Core::new(runtime.handle(), CoreConfig::load().with_memory_secrets());
    let mut signal = core.signal();
    let deadline = Instant::now() + RUN_FOR;
    loop {
        if core.pump() {
            print_view(&core);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        // Wait for a change on the runtime; `Core` stays off it.
        let changed = runtime.block_on(async {
            tokio::time::timeout(left, signal.changed())
                .await
                .unwrap_or(true)
        });
        if !changed {
            break;
        }
    }
    let stopped = core.block_until_stopped(STOP_TIMEOUT);
    println!("clients stopped: {stopped}");
}

fn print_view(core: &Core) {
    let view = core.view();
    println!("status: {}", view.status_text);
    for account in &view.accounts {
        println!(
            "  {:<9} {}",
            account.caps.id.display_name(),
            account.status.as_str()
        );
    }
}
