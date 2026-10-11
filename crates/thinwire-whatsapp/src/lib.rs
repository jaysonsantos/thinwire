// SPDX-License-Identifier: AGPL-3.0-only
//! WhatsApp linked-device adapter, and the `thinwire-whatsapp-helper`
//! program that runs it.
//!
//! This crate is AGPL-3.0-only, because the live client (feature
//! `whatsapp-web`) links whatsapp-rust and its AGPL `wacore-libsignal`. No
//! MIT crate of this workspace depends on it. The MIT thinwire app starts
//! the helper program as a child process and talks to it over a pipe
//! (ADR 0013). `scripts/check-agpl-deps.sh` keeps it that way.
//!
//! The adapter moved here from `thinwire-protocol` in #77. That crate keeps
//! an MIT stub, the helper adapter, and the shared
//! [`thinwire_protocol::WhatsAppPhoneVault`].

mod whatsapp;

pub use whatsapp::{
    SessionLock, SessionLockError, WhatsAppAdapter, lock_session, parse_whatsapp_chat_id,
    set_session_dir,
};
