//! NexaDesk transport. See docs/DETAILED_DESIGN.md §4 and §6.2-6.3.

pub mod cc;
pub mod check;
pub mod gather;
pub mod path;
pub mod rate;
pub mod recovery;
pub mod shard;
#[cfg(any(test, feature = "sim"))]
pub mod sim;
pub mod stun;
pub mod udp;
pub mod vsock;
