// SPDX-License-Identifier: AGPL-3.0-only
//! Signal linked-device adapter, and the `thinwire-signal-helper` program
//! that runs it.
//!
//! This crate is AGPL-3.0-only, because the live client (feature
//! `signal-local`) links Presage and libsignal. No MIT crate of this
//! workspace depends on it. The MIT thinwire app starts the helper program
//! as a child process and talks to it over a pipe (ADR 0013, #248).
//! `scripts/check-agpl-deps.sh` keeps it that way.
//!
//! `thinwire-protocol` keeps an MIT stub and the helper adapter.

mod signal;

pub use signal::{SessionLock, SessionLockError, SignalAdapter, lock_session, set_session_dir};
