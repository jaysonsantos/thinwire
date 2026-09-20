//! Protocol adapters and the egui shell for thinwire.
//!
//! Protocol I/O belongs on a tokio worker. The UI only polls
//! [`crate::protocols::AdapterEvent`] values from a channel.

#![forbid(unsafe_code)]

pub mod app;
pub mod protocols;

pub use protocols::{
    AdapterCommand, AdapterError, AdapterEvent, AdapterStatus, DiscordAuthMode, ProtocolAdapter,
    ProtocolCapabilities, ProtocolId, SupportClass,
};
