//! Protocols that run in a helper process (ADR 0012 section 2, ADR 0013).
//!
//! WhatsApp and Signal need AGPL code, and the MIT app must not link it. So
//! each of them runs in its own helper program, and the app talks to it over
//! the wire protocol of `thinwire-ipc`.
//!
//! - App side: [`HelperAdapter`] implements [`crate::ProtocolAdapter`]. The
//!   host routes commands to it like to any other adapter.
//! - Helper side: [`serve`] runs an adapter behind the helper's stdin and
//!   stdout.
//!
//! Both sides are MIT. This module names no AGPL crate.

mod convert;
mod launch;
mod serve;
mod supervisor;
#[cfg(test)]
mod tests;

pub use launch::{HelperInput, HelperLauncher, HelperOutput, HelperProcess, ProcessLauncher};
pub use serve::{ServeConfig, ServeEnd, refuse, serve};
pub use supervisor::{ADAPTER_STOP_WAIT, HelperAdapter, HelperSpec, HelperTiming};
pub use thinwire_ipc::HelperRefusal;

/// True for a log target that a helper program can write to its stderr:
/// a target of thinwire code.
///
/// The protocol libraries log pairing data at level `info`: whatsapp-rust
/// logs the pair code, and presage logs the provisioning URL. The app reads
/// the stderr of a helper and logs each line again. So a helper drops the
/// log lines of every other target, and no `RUST_LOG` value turns them on.
#[must_use]
pub fn helper_log_target(target: &str) -> bool {
    target.starts_with("thinwire")
}

#[cfg(test)]
mod log_target_tests {
    use super::helper_log_target;

    #[test]
    fn a_helper_logs_thinwire_targets_only() {
        for own in [
            "thinwire_whatsapp::whatsapp::link",
            "thinwire_whatsapp_helper",
            "thinwire_signal::signal::live",
            "thinwire_protocol::helper::serve",
            "thinwire_ipc::frame",
        ] {
            assert!(helper_log_target(own), "{own}");
        }
        for library in [
            // whatsapp-rust logs "Pair code generated: ..." with this target.
            "Bot/PairCode",
            "Client/PairCode",
            "whatsapp_rust::pair",
            "wacore::libsignal",
            // presage logs the provisioning URL.
            "presage::manager::linking",
            "presage_store_sqlite",
            "libsignal_service::provisioning",
            "libsignal_protocol",
            "reqwest::connect",
            "hyper::proto",
            "",
        ] {
            assert!(!helper_log_target(library), "{library}");
        }
    }
}
