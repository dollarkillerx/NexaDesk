//! TURN on the first STUN socket (`nexa_turn`), for browsers across the internet.
//!
//! Binding requests still get the plain STUN responder's answer, rate-limited as on
//! any STUN socket. TURN requests are rate-limited the same way; relayed data is not,
//! since it only ever moves between two allocations the server made (see
//! `nexa_turn::server`).

use std::{
    net::IpAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use nexa_net::udp::UdpIo;
use nexa_turn::msg::{Class, Msg};

use crate::limit::{Keyed, Rate};

const MAX_TRACKED_IPS: usize = 65_536;
const SWEEP_EVERY: Duration = Duration::from_secs(5);

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub async fn serve(socket: Arc<UdpIo>, turn: Arc<Mutex<nexa_turn::Server>>, rate: Rate) {
    let mut limiter: Keyed<IpAddr> = Keyed::new(rate, MAX_TRACKED_IPS);
    let mut buf = vec![0u8; 2048];
    let mut swept = Instant::now();
    loop {
        let (n, from) = match socket.recv_from(&mut buf).await {
            Ok(r) => r,
            Err(_) => continue,
        };
        let packet = &buf[..n];
        let now = Instant::now();
        let request = Msg::parse(packet).is_some_and(|m| m.class == Class::Request);
        if request && !limiter.allow(&from.ip().to_canonical(), now) {
            continue;
        }
        if let Some(reply) = crate::stun::answer(packet, from) {
            let _ = socket.try_send_to(&reply, from);
            continue;
        }
        let out = {
            let mut t = turn.lock().unwrap_or_else(|e| e.into_inner());
            if now.saturating_duration_since(swept) >= SWEEP_EVERY {
                t.sweep(now);
                swept = now;
            }
            t.handle(from, packet, now, unix_now())
        };
        for (to, data) in out {
            let _ = socket.try_send_to(&data, to);
        }
    }
}
