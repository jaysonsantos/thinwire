//! Wire protocol between the MIT thinwire app and a helper process
//! (ADR 0012 section 2, ADR 0013).
//!
//! WhatsApp and Signal need AGPL code. Each runs in its own helper program.
//! The app starts the helper as a child process and talks to it only through
//! the lines in this crate: one JSON object per line on the helper's stdin
//! and stdout. The helper writes its logs to stderr.
//!
//! The wire types are separate from `AdapterCommand` and `AdapterEvent` of
//! `thinwire-protocol`, and this crate does not depend on that crate. So a
//! change inside the app does not change the wire protocol, and the lines
//! stay at the level of user data: chats, messages, statuses.
//!
//! A change to a wire type that an older peer cannot read must raise
//! [`PROTOCOL_VERSION`].

mod frame;
mod secret;
mod wire;

pub use frame::{FrameError, FrameReader, MAX_LINE_BYTES, decode, encode, write_line};
pub use secret::SecretText;
pub use wire::{
    AppLine, HelperLine, HelperRefusal, PROTOCOL_VERSION, WireAccountState, WireArrival,
    WireCommand, WireConversation, WireDelivery, WireEvent, WireMessage, WireProtocol, WireStatus,
};
