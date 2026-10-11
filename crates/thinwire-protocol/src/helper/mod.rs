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
