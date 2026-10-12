//! IRC protocol client used by the `cricr` TUI.
//!
//! This crate has no terminal dependencies: it parses and serializes IRC
//! messages, negotiates IRCv3 capabilities, authenticates with SASL, and
//! drives a connection over TCP or TLS, reporting semantic [`events::Event`]s
//! to the caller.

pub mod caps;
pub mod client;
pub mod commands;
pub mod events;
pub mod handlers;
pub mod message;
pub mod sasl;

pub use client::{spawn, ClientCert, ClientConfig, ClientHandle};
pub use events::{ClientCommand, Event};
pub use message::Message;
