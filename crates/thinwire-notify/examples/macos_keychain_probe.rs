//! L8 spike: does an ad-hoc signed rebuild of `Thinwire.app` read a keychain
//! item that the previous build saved, and does macOS ask first?
//!
//! Same store as thinwire-core on macOS: `apple-native-keyring-store` with the
//! `keychain` (legacy file keychain) feature. The service is a test name, so
//! real thinwire logins are never touched.
//!
//! Usage: `macos_keychain_probe <save|read|delete> <account>`. Build it with
//! `L8_BUILD=A` and `L8_BUILD=B` to get two binaries with different hashes.
//! Each step prints one `STEP <name>: <result>` line. `PROBE_KEYCHAIN_TIMEOUT`
//! limits a step in seconds (default 120), because a keychain prompt blocks.

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("STEP platform: not macOS, nothing to probe");
}

#[cfg(target_os = "macos")]
fn main() {
    std::process::exit(probe::run());
}

#[cfg(target_os = "macos")]
mod probe {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use keyring_core::Entry;

    /// Test service. Never the real `thinwire` service.
    const SERVICE: &str = "dev.jaysonsantos.thinwire.l8-probe";
    /// Compile-time build tag. A different tag changes the binary hash.
    const BUILD: &str = match option_env!("L8_BUILD") {
        Some(tag) => tag,
        None => "unset",
    };

    fn secret(account: &str) -> String {
        format!("l8-probe-secret-{account}")
    }

    fn timeout() -> Duration {
        let secs = std::env::var("PROBE_KEYCHAIN_TIMEOUT")
            .ok()
            .and_then(|secs| secs.parse().ok())
            .unwrap_or(120);
        Duration::from_secs(secs)
    }

    /// Run `work` on a thread. `None` when it does not finish within the
    /// timeout (for example an unanswered keychain prompt).
    fn bounded<T: Send + 'static>(
        work: impl FnOnce() -> T + Send + 'static,
    ) -> (Option<T>, Duration) {
        let (tx, rx) = mpsc::channel();
        let started = Instant::now();
        std::thread::spawn(move || {
            let _ = tx.send(work());
        });
        let result = rx.recv_timeout(timeout()).ok();
        (result, started.elapsed())
    }

    fn entry(account: &str) -> Result<Entry, String> {
        Entry::new(SERVICE, account).map_err(|error| format!("{error:?}"))
    }

    pub(super) fn run() -> i32 {
        let args: Vec<String> = std::env::args().collect();
        let (Some(mode), Some(account)) = (args.get(1), args.get(2)) else {
            println!("STEP keychain-{BUILD}-usage: err need <save|read|delete> <account>");
            return 2;
        };
        let account = account.clone();
        println!(
            "STEP keychain-{BUILD}-build: tag {BUILD}, pid {}",
            std::process::id()
        );

        match apple_native_keyring_store::keychain::Store::new() {
            Ok(store) => keyring_core::set_default_store(store),
            Err(error) => {
                println!("STEP keychain-{BUILD}-store: err {error:?}");
                return 1;
            }
        }

        match mode.as_str() {
            "save" => {
                let value = secret(&account);
                let (result, elapsed) = bounded(move || {
                    entry(&account)?
                        .set_password(&value)
                        .map_err(|e| format!("{e:?}"))
                });
                report(
                    &format!("keychain-{BUILD}-save"),
                    result.map(|r| r.map(|()| "ok")),
                    elapsed,
                )
            }
            "read" => {
                let expected = secret(&account);
                let (result, elapsed) = bounded(move || {
                    entry(&account)?
                        .get_password()
                        .map_err(|e| format!("{e:?}"))
                });
                let name = format!("keychain-{BUILD}-read");
                let code = match result {
                    Some(Ok(value)) if value == expected => {
                        println!("STEP {name}: ok");
                        0
                    }
                    Some(Ok(_)) => {
                        println!("STEP {name}: mismatch");
                        1
                    }
                    Some(Err(error)) => {
                        println!("STEP {name}: err {error}");
                        1
                    }
                    None => {
                        println!("STEP {name}: err timeout after {}s", timeout().as_secs());
                        1
                    }
                };
                println!("STEP {name}-ms: {}", elapsed.as_millis());
                code
            }
            "delete" => {
                let (result, elapsed) = bounded(move || {
                    entry(&account)?
                        .delete_credential()
                        .map_err(|e| format!("{e:?}"))
                });
                report(
                    "keychain-cleanup",
                    result.map(|r| r.map(|()| format!("ok (by build {BUILD})"))),
                    elapsed,
                )
            }
            other => {
                println!("STEP keychain-{BUILD}-usage: err unknown mode {other}");
                2
            }
        }
    }

    fn report<T: std::fmt::Display>(
        name: &str,
        result: Option<Result<T, String>>,
        elapsed: Duration,
    ) -> i32 {
        let code = match result {
            Some(Ok(value)) => {
                println!("STEP {name}: {value}");
                0
            }
            Some(Err(error)) => {
                println!("STEP {name}: err {error}");
                1
            }
            None => {
                println!("STEP {name}: err timeout after {}s", timeout().as_secs());
                1
            }
        };
        println!("STEP {name}-ms: {}", elapsed.as_millis());
        code
    }
}
