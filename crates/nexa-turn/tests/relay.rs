//! A server and two clients (a host and a browser's stand-in) moving datagrams.

use std::{
    net::{IpAddr, SocketAddr},
    time::{Duration, Instant},
};

use nexa_turn::{
    Client, ClientEvent, Server, ServerConfig, auth,
    msg::{self, Builder, Class},
};

const SERVER: &str = "203.0.113.1:3478";
const UNIX: u64 = 1_800_000_000;

fn server() -> Server {
    Server::new(
        ServerConfig {
            realm: "nexadesk".into(),
            secret: b"secret".to_vec(),
            relay_ip: "203.0.113.1".parse().unwrap(),
            max_allocations: 10,
            max_per_ip: 2,
        },
        [9; 32],
    )
}

struct End {
    addr: SocketAddr,
    client: Client,
    got: Vec<ClientEvent>,
}

impl End {
    fn new(addr: &str, creds: auth::Credentials, now: Instant) -> Self {
        Self {
            addr: addr.parse().unwrap(),
            client: Client::new(SERVER.parse().unwrap(), creds, now),
            got: Vec::new(),
        }
    }
    fn relayed(&self) -> Option<SocketAddr> {
        self.got.iter().find_map(|e| match e {
            ClientEvent::Allocated(a) => Some(*a),
            _ => None,
        })
    }
    fn data(&self) -> Vec<Vec<u8>> {
        self.got
            .iter()
            .filter_map(|e| match e {
                ClientEvent::Data { data, .. } => Some(data.clone()),
                _ => None,
            })
            .collect()
    }
}

/// Moves everything until nothing moves. Returns datagrams the server sent anywhere
/// other than the two ends.
fn pump(s: &mut Server, ends: &mut [&mut End], now: Instant) -> Vec<(SocketAddr, Vec<u8>)> {
    let mut stray = Vec::new();
    for _ in 0..100 {
        let mut moved = false;
        for i in 0..ends.len() {
            while let Some(d) = ends[i].client.poll_transmit() {
                moved = true;
                let from = ends[i].addr;
                for (to, out) in s.handle(from, &d, now, UNIX) {
                    match ends.iter_mut().find(|e| e.addr == to) {
                        Some(e) => e.client.handle(&out, now),
                        None => stray.push((to, out)),
                    }
                }
            }
            while let Some(ev) = ends[i].client.poll_event() {
                ends[i].got.push(ev);
            }
        }
        if !moved {
            break;
        }
    }
    stray
}

fn creds(label: &str) -> auth::Credentials {
    auth::issue(b"secret", label, UNIX + 3600)
}

#[test]
fn two_allocations_talk_and_nothing_leaves() {
    let now = Instant::now();
    let mut s = server();
    let mut host = End::new("198.51.100.7:50000", creds("host"), now);
    let mut browser = End::new("192.0.2.44:61000", creds("browser"), now);
    assert!(pump(&mut s, &mut [&mut host, &mut browser], now).is_empty());
    let (h, b) = (host.relayed().unwrap(), browser.relayed().unwrap());
    assert_eq!(h.ip(), "203.0.113.1".parse::<IpAddr>().unwrap());
    assert_ne!(h.port(), b.port());

    // Without the host's permission the browser's data goes nowhere.
    browser.client.permit(h.ip(), now);
    browser.client.send(h, b"too early", now);
    pump(&mut s, &mut [&mut host, &mut browser], now);
    assert!(host.data().is_empty());

    host.client.permit(b.ip(), now);
    pump(&mut s, &mut [&mut host, &mut browser], now);
    browser.client.send(h, b"ice check", now);
    pump(&mut s, &mut [&mut host, &mut browser], now);
    assert_eq!(host.data(), vec![b"ice check".to_vec()]);
    assert!(matches!(host.got.last(), Some(ClientEvent::Data { from, .. }) if *from == b));

    // Back the other way; by now the browser's channel is bound, then the host's.
    host.client.send(b, b"answer", now);
    pump(&mut s, &mut [&mut host, &mut browser], now);
    for i in 0..50u8 {
        host.client.send(b, &[i; 1100], now);
        browser.client.send(h, &[i; 60], now);
    }
    pump(&mut s, &mut [&mut host, &mut browser], now);
    assert_eq!(browser.data().len(), 51);
    assert_eq!(host.data().len(), 51);
    assert_eq!(s.stats().relayed_packets, 102);

    // An open relay it is not: data for an outside address is dropped.
    browser.client.permit("8.8.8.8".parse().unwrap(), now);
    browser
        .client
        .send("8.8.8.8:53".parse().unwrap(), b"dns", now);
    assert!(pump(&mut s, &mut [&mut host, &mut browser], now).is_empty());
}

#[test]
fn wrong_or_expired_credentials_get_nothing() {
    let now = Instant::now();
    let mut s = server();
    let mut forged = End::new(
        "192.0.2.1:1000",
        auth::Credentials {
            username: format!("{}:x", UNIX + 3600),
            password: "guess".into(),
        },
        now,
    );
    let mut expired = End::new(
        "192.0.2.2:1000",
        auth::issue(b"secret", "old", UNIX - 1),
        now,
    );
    pump(&mut s, &mut [&mut forged, &mut expired], now);
    for end in [&forged, &expired] {
        assert!(end.relayed().is_none());
        assert!(
            matches!(end.got.last(), Some(ClientEvent::Failed(_))),
            "{:?}",
            end.got
        );
    }
    assert_eq!(s.stats().allocations, 0);
}

#[test]
fn allocations_per_address_are_limited() {
    let now = Instant::now();
    let mut s = server();
    let mut a = End::new("192.0.2.9:1", creds("a"), now);
    let mut b = End::new("192.0.2.9:2", creds("b"), now);
    let mut c = End::new("192.0.2.9:3", creds("c"), now);
    pump(&mut s, &mut [&mut a, &mut b, &mut c], now);
    assert!(a.relayed().is_some() && b.relayed().is_some());
    assert!(c.relayed().is_none());
}

#[test]
fn allocations_expire_and_refresh() {
    let start = Instant::now();
    let mut s = server();
    let mut host = End::new("198.51.100.7:50000", creds("host"), start);
    pump(&mut s, &mut [&mut host], start);
    assert_eq!(s.stats().allocations, 1);
    // The client refreshes on its own before the 600 s lifetime ends.
    let mut now = start;
    for _ in 0..20 {
        now += Duration::from_secs(60);
        host.client.handle_timeout(now);
        pump(&mut s, &mut [&mut host], now);
        s.sweep(now);
    }
    assert_eq!(s.stats().allocations, 1);
    // Abandoned, it goes.
    s.sweep(now + Duration::from_secs(3601));
    assert_eq!(s.stats().allocations, 0);
}

#[test]
fn binding_requests_are_answered() {
    let mut s = server();
    let from: SocketAddr = "192.0.2.5:4444".parse().unwrap();
    let req = Builder::new(msg::BINDING, Class::Request, [1; 12]).build();
    let out = s.handle(from, &req, Instant::now(), UNIX);
    let m = msg::Msg::parse(&out[0].1).unwrap();
    assert_eq!(m.class, Class::Success);
    assert_eq!(m.addr(msg::XOR_MAPPED_ADDRESS), Some(from));
}
