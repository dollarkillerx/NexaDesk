//! The STUN responder (§4.2): answers Binding requests with the source address.
//!
//! Answers are at most 1.6 times the request, and rate-limited per source IP,
//! so the service is a poor amplifier.

use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Instant,
};

use nexa_net::{
    stun::{StunClass, StunMessage, encode_binding_success},
    udp::UdpIo,
};

use crate::limit::{Keyed, Rate};

const MAX_TRACKED_IPS: usize = 65_536;

/// The answer to one datagram, if it deserves one.
pub fn answer(packet: &[u8], from: SocketAddr) -> Option<Vec<u8>> {
    let msg = StunMessage::parse(packet).ok()?;
    if msg.class != StunClass::Request || msg.unknown_required.is_some() {
        return None;
    }
    encode_binding_success(&msg.txid, from, None).ok()
}

pub async fn serve(socket: Arc<UdpIo>, rate: Rate) {
    let mut limiter: Keyed<IpAddr> = Keyed::new(rate, MAX_TRACKED_IPS);
    let mut buf = vec![0u8; 2048];
    loop {
        let (n, from) = match socket.recv_from(&mut buf).await {
            Ok(r) => r,
            // ICMP-induced errors and the like: keep serving.
            Err(_) => continue,
        };
        if !limiter.allow(&from.ip().to_canonical(), Instant::now()) {
            continue;
        }
        if let Some(reply) = answer(&buf[..n], from) {
            let _ = socket.try_send_to(&reply, from);
        }
    }
}

#[cfg(test)]
mod tests {
    use nexa_net::stun::{TransactionId, encode_binding_request};

    use super::*;

    #[test]
    fn answers_binding_requests_only() {
        let from: SocketAddr = "198.51.100.4:5555".parse().unwrap();
        let txid = TransactionId([3; 12]);
        let req = encode_binding_request(&txid, None, None).unwrap();
        let reply = answer(&req, from).unwrap();
        let m = StunMessage::parse(&reply).unwrap();
        assert_eq!(
            (m.class, m.txid, m.mapped),
            (StunClass::Success, txid, Some(from))
        );
        assert!(reply.len() <= req.len() * 2);
        assert!(answer(&reply, from).is_none(), "responses are not answered");
        assert!(answer(b"\x00\x01garbage", from).is_none());
        assert!(answer(&[], from).is_none());
    }
}
