//! QUIC configuration (§4.8).
//!
//! quinn's congestion window and pacer are switched off because all rate
//! control happens in the application. M0 measured why this is required: with
//! quinn's default Cubic at 2% loss only 48 of 1200 frames were ever sent.

use std::{
    any::Any,
    sync::Arc,
    time::{Duration, Instant},
};

use quinn::{
    AckFrequencyConfig, TransportConfig, VarInt,
    congestion::{Controller, ControllerFactory},
};

/// quinn skips pacing when the window exceeds `u32::MAX`.
const UNLIMITED_WINDOW: u64 = 1 << 40;

pub const FIXED_MTU: u16 = 1200;
pub const DATAGRAM_SEND_BUFFER: usize = 512 * 1024;
pub const DATAGRAM_RECV_BUFFER: usize = 4 << 20;

#[derive(Clone, Debug, Default)]
pub struct MediaController;

impl Controller for MediaController {
    fn on_congestion_event(&mut self, _: Instant, _: Instant, _: bool, _: u64) {}
    fn on_mtu_update(&mut self, _: u16) {}
    fn window(&self) -> u64 {
        UNLIMITED_WINDOW
    }
    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(self.clone())
    }
    fn initial_window(&self) -> u64 {
        UNLIMITED_WINDOW
    }
    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

#[derive(Debug, Default)]
pub struct MediaControllerFactory;

impl ControllerFactory for MediaControllerFactory {
    fn build(self: Arc<Self>, _: Instant, _: u16) -> Box<dyn Controller> {
        Box::new(MediaController)
    }
}

pub fn transport_config() -> Arc<TransportConfig> {
    let mut t = TransportConfig::default();
    t.congestion_controller_factory(Arc::new(MediaControllerFactory))
        .initial_mtu(FIXED_MTU)
        .min_mtu(FIXED_MTU)
        // Path changes are invisible to QUIC, so a probed MTU would black-hole on Tailscale.
        .mtu_discovery_config(None)
        .datagram_send_buffer_size(DATAGRAM_SEND_BUFFER)
        .datagram_receive_buffer_size(Some(DATAGRAM_RECV_BUFFER))
        .max_idle_timeout(Some(Duration::from_secs(10).try_into().expect("in range")))
        .keep_alive_interval(Some(Duration::from_secs(2)))
        .enable_segmentation_offload(true);
    let mut ack = AckFrequencyConfig::default();
    ack.ack_eliciting_threshold(VarInt::from_u32(16))
        .max_ack_delay(Some(Duration::from_millis(10)));
    t.ack_frequency_config(Some(ack));
    Arc::new(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_disables_quinn_pacing() {
        assert!(MediaController.window() > u64::from(u32::MAX));
        assert!(MediaController.initial_window() > u64::from(u32::MAX));
    }
}
