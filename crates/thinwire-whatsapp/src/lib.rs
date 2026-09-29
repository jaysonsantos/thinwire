// SPDX-License-Identifier: AGPL-3.0-only
//! Local-only WhatsApp linked-device adapter.
//!
//! This crate is AGPL-3.0-only, because the live client links whatsapp-rust
//! and its AGPL `wacore-libsignal` (ADR 0011). `thinwire` depends on it only
//! with feature `whatsapp-web`. `thinwire-protocol` does not depend on it.
//! Release builds and OS zips do not enable that feature.
//!
//! The adapter moved here from `thinwire-protocol` in #77. That crate keeps
//! an MIT stub and the shared [`thinwire_protocol::WhatsAppPhoneVault`].

mod whatsapp;

pub use whatsapp::{WhatsAppAdapter, parse_whatsapp_chat_id};
