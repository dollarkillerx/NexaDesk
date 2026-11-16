//! TURN for NexaDesk's browser access across the internet (docs/WEB_CLIENT.md §12).
//!
//! A browser can only use standard ICE servers, so when neither end can reach the other,
//! both the browser and the host allocate a relay on the signaling server and talk
//! through it. This crate is that TURN server and the host's TURN client, as pure state
//! machines: datagrams in, datagrams out, the time passed in. No sockets, no clock.
//!
//! The server is deliberately not an open relay: its relayed addresses are virtual, and
//! data only ever moves between two allocations on the same server. Nothing can be sent
//! through it to the rest of the internet.

pub mod auth;
pub mod client;
pub mod msg;
pub mod server;

pub use auth::Credentials;
pub use client::{Client, ClientEvent};
pub use server::{Server, ServerConfig};
