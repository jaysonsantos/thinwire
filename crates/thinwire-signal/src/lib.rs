// SPDX-License-Identifier: AGPL-3.0-only
//! Local-only Signal adapter.
//!
//! This crate is AGPL-3.0-only. `thinwire` depends on it only with feature
//! `signal-local`. `thinwire-protocol` does not depend on it. Release builds
//! and OS zips do not enable that feature.

mod signal;

pub use signal::SignalAdapter;
