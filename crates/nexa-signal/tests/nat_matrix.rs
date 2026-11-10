//! Connecting by ID across simulated NATs (§4, §14): the real signaling server,
//! STUN and relay, the real rendezvous, checks and QUIC, on an in-process
//! network. Nothing leaves this process except signaling on 127.0.0.1.
//!
//! Run with `--nocapture` to see the matrix with connection times.

use std::{
    collections::HashSet,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};

use nexa_auth::{
    DeviceIdentity, PeerPolicy, client_config, peer_public_key, server_config, tls::SERVER_NAME,
};
use nexa_net::{
    check::PathClass,
    sim::{NatConfig, SimNet},
    udp::UdpIo,
    vsock::{PathTarget, VIRTUAL_PEER},
};
use nexa_session::{
    framed::{accept_bi, open_bi},
    proto::{ControlBody, ControlMsg},
    rendezvous::{
        self, Dialed, HostRendezvous, Stage,
        link::{Interfaces, LinkOptions, SocketFactory},
        open_incoming,
        seal::SignalKeys,
        signal::{SignalClient, SignalConfig},
    },
};
use nexa_signal::{Settings, Sockets, config::Limits, relay::RelayLimits};
use nexa_wire::{control::PathSwitch, framing::StreamType, signal::ServerBody};
use quinn::AsyncUdpSocket;

fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

const STUN_A: &str = "203.0.113.1:3478";
const STUN_B: &str = "203.0.113.2:3478";
const RELAY: &str = "203.0.113.1:3480";
const VIEWER_LAN: &str = "192.168.1.10";
const HOST_LAN: &str = "10.0.0.20";
const VIEWER_PUBLIC: &str = "198.51.100.1";
const HOST_PUBLIC: &str = "198.51.100.2";

fn relaxed_limits() -> Limits {
    // Every test client shares 127.0.0.1, which the per-IP limits would otherwise count once.
    Limits {
        register_per_ip: (100.0, 100.0),
        lookup_per_ip: (100.0, 100.0),
        connect_per_ip: (100.0, 100.0),
        ..Limits::default()
    }
}

struct World {
    net: SimNet,
    server: nexa_signal::Server,
    signal: SignalConfig,
}

#[derive(Clone)]
enum Nat {
    None,
    Easy,
    Hard,
    HardRandom(u64),
    /// Outbound UDP reaches only the server's addresses.
    Firewalled,
}

impl Nat {
    fn config(&self, public: IpAddr, uplink_ms: u64) -> Option<NatConfig> {
        let mut c = match self {
            Nat::None => return None,
            Nat::Easy => NatConfig::easy(public),
            Nat::Hard => NatConfig::hard(public),
            Nat::HardRandom(seed) => NatConfig::hard_random(public, *seed),
            Nat::Firewalled => {
                let mut c = NatConfig::easy(public);
                c.allow_only = Some(server_ips());
                c
            }
        };
        c.uplink = Duration::from_millis(uplink_ms);
        Some(c)
    }
}

fn server_ips() -> HashSet<IpAddr> {
    [ip("203.0.113.1"), ip("203.0.113.2")].into()
}

async fn world(tls: bool) -> World {
    let net = SimNet::new();
    net.add_public_host(ip("203.0.113.1"), Duration::from_millis(20));
    net.add_public_host(ip("203.0.113.2"), Duration::from_millis(20));
    let stun = vec![
        Arc::new(UdpIo::new(net.bind(STUN_A.parse().unwrap()).unwrap())),
        Arc::new(UdpIo::new(net.bind(STUN_B.parse().unwrap()).unwrap())),
    ];
    let relay = Arc::new(UdpIo::new(net.bind(RELAY.parse().unwrap()).unwrap()));
    let ws = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (tls_cfg, mut signal) = if tls {
        let c = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
        let cert = rustls::pki_types::CertificateDer::from(c.cert.der().to_vec());
        let key =
            rustls::pki_types::PrivateKeyDer::try_from(c.signing_key.serialize_der()).unwrap();
        let cfg = nexa_signal::tls_from_der(vec![cert.clone()], key).unwrap();
        let mut s = SignalConfig::new(String::new());
        s.extra_roots = vec![cert];
        s.only_extra_roots = true;
        (Some(cfg), s)
    } else {
        (None, SignalConfig::new(String::new()))
    };
    let server = nexa_signal::start(
        Settings {
            tls: tls_cfg,
            server_names: vec![if tls { "localhost" } else { "127.0.0.1" }.to_owned()],
            stun_advertise: vec![STUN_A.parse().unwrap(), STUN_B.parse().unwrap()],
            relay_advertise: Some(RELAY.parse().unwrap()),
            relay: RelayLimits {
                bandwidth_bps: 100_000_000,
                max_sessions: 100,
            },
            relay_token_ttl_secs: 600,
            limits: relaxed_limits(),
        },
        Sockets {
            ws,
            stun,
            relay: Some(relay),
        },
    )
    .await
    .unwrap();
    let port = server.ws_addr.port();
    signal.url = if tls {
        format!("wss://localhost:{port}")
    } else {
        format!("ws://127.0.0.1:{port}")
    };
    World {
        net,
        server,
        signal,
    }
}

fn options(net: &SimNet, lan_ip: IpAddr) -> LinkOptions {
    let net = net.clone();
    let factory: SocketFactory = Arc::new(move |_| {
        let s: Arc<dyn AsyncUdpSocket> = net.bind(SocketAddr::new(lan_ip, 0))?;
        Ok(s)
    });
    LinkOptions {
        bind: SocketAddr::new(lan_ip, 0),
        interfaces: Interfaces::Fixed(vec![lan_ip]),
        relay_only: false,
        socket_factory: Some(factory),
        repunch_every: Duration::from_secs(2),
    }
}

struct Device {
    identity: Arc<DeviceIdentity>,
    keys: Arc<SignalKeys>,
}

fn device() -> Device {
    let identity = Arc::new(DeviceIdentity::generate());
    let keys = Arc::new(SignalKeys::new(identity.clone()));
    Device { identity, keys }
}

/// A host that answers every Incoming on its signaling connection.
struct Host {
    id: String,
    accepted: tokio::sync::mpsc::UnboundedReceiver<(HostRendezvous, quinn::Connection)>,
    _task: tokio::task::JoinHandle<()>,
}

async fn start_host(w: &World, dev: &Device, opts: LinkOptions) -> Host {
    let mut client = SignalClient::connect(&w.signal, &dev.keys).await.unwrap();
    let id = client.registration.short_id.clone();
    let stun = client.registration.stun_servers.clone();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let keys = dev.keys.clone();
    let identity = dev.identity.clone();
    let task = tokio::spawn(async move {
        let mut sessions: Vec<HostRendezvous> = Vec::new();
        let out = client.sender();
        loop {
            match client.recv().await {
                Some(ServerBody::Incoming(i)) => {
                    let Ok(offered) = open_incoming(&keys, &i) else {
                        continue;
                    };
                    let server = server_config(&identity, PeerPolicy::AcceptUnknown).unwrap();
                    let (out, keys, stun, opts, tx) = (
                        out.clone(),
                        keys.clone(),
                        stun.clone(),
                        opts.clone(),
                        tx.clone(),
                    );
                    tokio::spawn(async move {
                        let r = rendezvous::answer(out, keys, offered, &stun, opts, server)
                            .await
                            .unwrap();
                        let incoming =
                            tokio::time::timeout(Duration::from_secs(20), r.link.endpoint.accept())
                                .await
                                .ok()
                                .flatten();
                        if let Some(incoming) = incoming
                            && let Ok(conn) = incoming.await
                        {
                            let _ = tx.send((r, conn));
                        }
                    });
                }
                Some(ServerBody::Trickle(t)) => {
                    for s in &sessions {
                        if t.session_id == s.session {
                            let _ = s.trickles.send(t.clone());
                        }
                    }
                }
                Some(_) => {}
                None => break,
            }
            sessions.retain(|s| !s.pump.is_finished());
        }
    });
    Host {
        id,
        accepted: rx,
        _task: task,
    }
}

struct Outcome {
    validated_after: Duration,
    quic_after: Duration,
    first: PathClass,
    dialed: Dialed,
    viewer_conn: quinn::Connection,
    host: (HostRendezvous, quinn::Connection),
}

async fn connect(
    w: &World,
    viewer: &Device,
    host_dev: &Device,
    host: &mut Host,
    opts: LinkOptions,
) -> Result<Outcome, String> {
    let t0 = Instant::now();
    let dialed = rendezvous::dial(
        &w.signal,
        &viewer.keys,
        &host.id,
        None,
        opts,
        &|_: Stage| {},
    )
    .await
    .map_err(|e| e.to_string())?;
    let validated_after = t0.elapsed();
    let first = dialed.first_path.class;
    let cfg = client_config(&viewer.identity, PeerPolicy::AcceptUnknown).unwrap();
    let conn = tokio::time::timeout(
        Duration::from_secs(10),
        dialed
            .link
            .endpoint
            .connect_with(cfg, VIRTUAL_PEER, SERVER_NAME)
            .unwrap(),
    )
    .await
    .map_err(|_| "QUIC timed out".to_owned())?
    .map_err(|e| e.to_string())?;
    let quic_after = t0.elapsed();
    // The TLS identity is the key the rendezvous was made with, on both sides.
    assert_eq!(
        peer_public_key(&conn).unwrap(),
        host_dev.identity.public_key()
    );
    assert_eq!(dialed.host_key, host_dev.identity.public_key());
    let host_side = tokio::time::timeout(Duration::from_secs(10), host.accepted.recv())
        .await
        .map_err(|_| "host never accepted".to_owned())?
        .ok_or("host task ended")?;
    assert_eq!(
        peer_public_key(&host_side.1).unwrap(),
        viewer.identity.public_key()
    );
    assert_eq!(host_side.0.viewer, viewer.identity.public_key());
    Ok(Outcome {
        validated_after,
        quic_after,
        first,
        dialed,
        viewer_conn: conn,
        host: host_side,
    })
}

/// Data over QUIC both ways, so we know the path carries it. Uni streams, so
/// as not to collide with the Control stream's bidirectional one.
async fn exchange(viewer: &quinn::Connection, host: &quinn::Connection) {
    let f = async {
        let mut s = viewer.open_uni().await.unwrap();
        s.write_all(b"ping").await.unwrap();
        s.finish().unwrap();
        let mut r = host.accept_uni().await.unwrap();
        assert_eq!(r.read_to_end(64).await.unwrap(), b"ping");
        let mut s = host.open_uni().await.unwrap();
        s.write_all(b"pong").await.unwrap();
        s.finish().unwrap();
        let mut r = viewer.accept_uni().await.unwrap();
        assert_eq!(r.read_to_end(64).await.unwrap(), b"pong");
    };
    tokio::time::timeout(Duration::from_secs(10), f)
        .await
        .expect("exchange timed out");
}

/// Carries the viewer's PathSwitch decisions to the host over a Control stream,
/// as the real session does.
fn wire_path_switches(o: &Outcome) -> tokio::task::JoinHandle<()> {
    let mut switches = o.dialed.link.take_switches().unwrap();
    let (vconn, hconn) = (o.viewer_conn.clone(), o.host.1.clone());
    let host_link = o.host.0.link.clone();
    tokio::spawn(async move {
        let (mut send, _recv) = open_bi(&vconn, StreamType::Control).await.unwrap();
        let host_side = tokio::spawn(async move {
            let (_, _, mut recv) = accept_bi(&hconn).await.unwrap();
            while let Ok(Some(m)) = recv.next::<ControlMsg>().await {
                if let Some(ControlBody::PathSwitch(p)) = m.body {
                    host_link.apply_remote_switch(p.path_id);
                }
            }
        });
        while let Some(tag) = switches.recv().await {
            if send
                .send(&ControlMsg::from(ControlBody::PathSwitch(PathSwitch {
                    path_id: tag,
                })))
                .await
                .is_err()
            {
                break;
            }
        }
        host_side.abort();
    })
}

fn class_name(c: PathClass) -> &'static str {
    match c {
        PathClass::Direct => "直连 direct",
        PathClass::Punched => "打洞 punched",
        PathClass::Relay => "中继 relay",
        PathClass::Tailscale => "Tailscale",
    }
}

/// Keeps QUIC busy, as a session's ClockPings would.
fn keep_busy(v: &quinn::Connection, h: &quinn::Connection) -> tokio::task::JoinHandle<()> {
    let (vc, hc) = (v.clone(), h.clone());
    tokio::spawn(async move {
        loop {
            let _ = vc.send_datagram(bytes::Bytes::from_static(b"tick"));
            let _ = hc.send_datagram(bytes::Bytes::from_static(b"tock"));
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
}

struct Report {
    first: PathClass,
    settled: PathClass,
    outcome: Outcome,
    world: World,
}

async fn scenario(name: &str, viewer_nat: Nat, host_nat: Nat, tls: bool) -> Report {
    tokio::time::timeout(
        Duration::from_secs(40),
        scenario_inner(name, viewer_nat, host_nat, tls),
    )
    .await
    .unwrap_or_else(|_| panic!("{name}: timed out"))
}

async fn scenario_inner(name: &str, viewer_nat: Nat, host_nat: Nat, tls: bool) -> Report {
    let w = world(tls).await;
    let (vlan, hlan) = match (&viewer_nat, &host_nat) {
        (Nat::None, _) => (ip(VIEWER_PUBLIC), ip(HOST_LAN)),
        (_, Nat::None) => (ip(VIEWER_LAN), ip(HOST_PUBLIC)),
        _ => (ip(VIEWER_LAN), ip(HOST_LAN)),
    };
    match viewer_nat.config(ip(VIEWER_PUBLIC), 10) {
        Some(c) => w.net.add_nat(c, &[vlan]),
        None => w.net.add_public_host(vlan, Duration::from_millis(10)),
    }
    match host_nat.config(ip(HOST_PUBLIC), 15) {
        Some(c) => w.net.add_nat(c, &[hlan]),
        None => w.net.add_public_host(hlan, Duration::from_millis(15)),
    }
    let (viewer, host_dev) = (device(), device());
    let mut host = start_host(&w, &host_dev, options(&w.net, hlan)).await;
    let o = connect(&w, &viewer, &host_dev, &mut host, options(&w.net, vlan))
        .await
        .unwrap_or_else(|e| panic!("{name}: {e}"));
    let _switches = wire_path_switches(&o);
    exchange(&o.viewer_conn, &o.host.1).await;
    // §5.3: use the first path at once, upgrade in the background. Give a relayed
    // start the hold time to move to a direct path, if one validated.
    let busy = keep_busy(&o.viewer_conn, &o.host.1);
    let mut settled_after = None;
    if o.first == PathClass::Relay {
        let mut watch = o.dialed.link.watch();
        let t = Instant::now();
        let up = tokio::time::timeout(
            Duration::from_secs(6),
            watch.wait_for(|s| s.active.is_some_and(|a| a.class != PathClass::Relay)),
        )
        .await
        .is_ok();
        if up {
            settled_after = Some(o.quic_after + t.elapsed());
        }
    }
    busy.abort();
    exchange(&o.viewer_conn, &o.host.1).await;
    let settled = o.dialed.link.state().active.map_or(o.first, |a| a.class);
    eprintln!(
        "MATRIX | {name:<38} | first {:<12} {:>5} ms validated, QUIC {:>5} ms | settled {:<12}{}",
        class_name(o.first),
        o.validated_after.as_millis(),
        o.quic_after.as_millis(),
        class_name(settled),
        settled_after.map_or(String::new(), |d| format!(" at {} ms", d.as_millis())),
    );
    std::mem::forget(host);
    Report {
        first: o.first,
        settled,
        outcome: o,
        world: w,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn easy_to_easy_punches_through() {
    let r = scenario("easy ↔ easy (TLS signaling)", Nat::Easy, Nat::Easy, true).await;
    let w = &r.world;
    assert_eq!(r.first, PathClass::Punched);
    assert!(matches!(
        r.outcome.dialed.first_path.target,
        PathTarget::Direct(_)
    ));
    // Nothing went through the relay but its bind traffic.
    // Only the relay pair's STUN checks crossed the relay, never QUIC (1200-byte packets).
    assert!(
        w.server.relay_stats().forwarded_bytes < 2048,
        "{:?}",
        w.server.relay_stats()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn easy_viewer_to_hard_host_punches_with_port_prediction() {
    let r = scenario(
        "easy viewer → hard host (seq. ports)",
        Nat::Easy,
        Nat::Hard,
        false,
    )
    .await;
    assert_eq!(r.settled, PathClass::Punched);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hard_viewer_to_easy_host_punches_with_port_prediction() {
    let r = scenario(
        "hard viewer (seq. ports) → easy host",
        Nat::Hard,
        Nat::Easy,
        false,
    )
    .await;
    assert_eq!(r.settled, PathClass::Punched);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn easy_to_random_hard_falls_back_to_relay() {
    let r = scenario(
        "easy viewer → hard host (random)",
        Nat::Easy,
        Nat::HardRandom(0xBEEF),
        false,
    )
    .await;
    assert_eq!((r.first, r.settled), (PathClass::Relay, PathClass::Relay));
    assert!(r.world.server.relay_stats().forwarded_bytes > 10_000);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hard_to_hard_falls_back_to_relay() {
    let r = scenario(
        "hard ↔ hard (random ports)",
        Nat::HardRandom(1),
        Nat::HardRandom(2),
        false,
    )
    .await;
    assert_eq!((r.first, r.settled), (PathClass::Relay, PathClass::Relay));
    assert!(r.world.server.relay_stats().forwarded_bytes > 10_000);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hard_to_hard_sequential_falls_back_to_relay() {
    let r = scenario(
        "hard ↔ hard (sequential ports)",
        Nat::Hard,
        Nat::Hard,
        false,
    )
    .await;
    assert_eq!((r.first, r.settled), (PathClass::Relay, PathClass::Relay));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn udp_blocked_except_to_the_server_uses_the_relay() {
    let r = scenario(
        "relay only (firewall: server only)",
        Nat::Firewalled,
        Nat::Firewalled,
        false,
    )
    .await;
    assert_eq!((r.first, r.settled), (PathClass::Relay, PathClass::Relay));
    assert!(r.world.server.relay_stats().forwarded_bytes > 10_000);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn public_host_is_reached_directly() {
    let r = scenario("easy viewer → public host", Nat::Easy, Nat::None, false).await;
    assert_eq!(r.settled, PathClass::Direct);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_lan_uses_host_candidates() {
    let w = world(false).await;
    let (a, b) = (ip("192.168.1.10"), ip("192.168.1.11"));
    let mut nat = NatConfig::easy(ip(VIEWER_PUBLIC));
    nat.uplink = Duration::from_millis(10);
    w.net.add_nat(nat, &[a, b]);
    let (viewer, host_dev) = (device(), device());
    let mut host = start_host(&w, &host_dev, options(&w.net, b)).await;
    let o = connect(&w, &viewer, &host_dev, &mut host, options(&w.net, a))
        .await
        .unwrap();
    exchange(&o.viewer_conn, &o.host.1).await;
    eprintln!(
        "MATRIX | {:<34} | first path {:<14} | validated {:>5} ms | QUIC up {:>5} ms",
        "same LAN behind one NAT",
        class_name(o.first),
        o.validated_after.as_millis(),
        o.quic_after.as_millis()
    );
    assert_eq!(o.first, PathClass::Direct);
    assert_eq!(
        o.dialed.first_path.target,
        PathTarget::Direct(SocketAddr::new(
            b,
            o.host.0.link.local_addr().unwrap().port()
        ))
    );
    std::mem::forget(host);
}

/// The host's "relay only" option: the viewer never learns a host address.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_only_host_never_reveals_its_address() {
    let w = world(false).await;
    w.net.add_nat(
        Nat::Easy.config(ip(VIEWER_PUBLIC), 10).unwrap(),
        &[ip(VIEWER_LAN)],
    );
    w.net.add_nat(
        Nat::Easy.config(ip(HOST_PUBLIC), 15).unwrap(),
        &[ip(HOST_LAN)],
    );
    let (viewer, host_dev) = (device(), device());
    let mut hopts = options(&w.net, ip(HOST_LAN));
    hopts.relay_only = true;
    let mut host = start_host(&w, &host_dev, hopts).await;
    let o = connect(
        &w,
        &viewer,
        &host_dev,
        &mut host,
        options(&w.net, ip(VIEWER_LAN)),
    )
    .await
    .unwrap();
    exchange(&o.viewer_conn, &o.host.1).await;
    assert_eq!(o.first, PathClass::Relay);
    // Wait past the re-punch interval: still nothing direct.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let st = o.dialed.link.state();
    assert!(
        st.validated.iter().all(|p| p.class == PathClass::Relay),
        "{:?}",
        st.validated
    );
    std::mem::forget(host);
}

/// §4.5: start on the relay, let the direct path open, upgrade after the hold,
/// and have the host follow the PathSwitch. Then fail back over.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upgrades_from_relay_and_fails_over_back() {
    let w = world(false).await;
    w.net.add_nat(
        Nat::Firewalled.config(ip(VIEWER_PUBLIC), 10).unwrap(),
        &[ip(VIEWER_LAN)],
    );
    w.net.add_nat(
        Nat::Easy.config(ip(HOST_PUBLIC), 15).unwrap(),
        &[ip(HOST_LAN)],
    );
    let (viewer, host_dev) = (device(), device());
    let mut host = start_host(&w, &host_dev, options(&w.net, ip(HOST_LAN))).await;
    let o = connect(
        &w,
        &viewer,
        &host_dev,
        &mut host,
        options(&w.net, ip(VIEWER_LAN)),
    )
    .await
    .unwrap();
    assert_eq!(o.first, PathClass::Relay);
    let _pump = wire_path_switches(&o);
    exchange(&o.viewer_conn, &o.host.1).await;

    // Keep QUIC busy, as the session would with ClockPings.
    let (vc, hc) = (o.viewer_conn.clone(), o.host.1.clone());
    let traffic = tokio::spawn(async move {
        loop {
            let _ = vc.send_datagram(bytes::Bytes::from_static(b"tick"));
            let _ = hc.send_datagram(bytes::Bytes::from_static(b"tock"));
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });

    let opened = Instant::now();
    w.net.set_firewall(ip(VIEWER_PUBLIC), None);
    let mut watch = o.dialed.link.watch();
    let upgraded = tokio::time::timeout(
        Duration::from_secs(15),
        watch.wait_for(|s| s.active.is_some_and(|a| a.class == PathClass::Punched)),
    )
    .await
    .is_ok();
    assert!(upgraded, "no upgrade: {:?}", o.dialed.link.state());
    let took = opened.elapsed();
    let viewer_target = o.dialed.link.vsock.active_path().unwrap();
    // The host follows the PathSwitch onto the same path.
    let host_link = o.host.0.link.clone();
    let deadline = Instant::now() + Duration::from_secs(3);
    while host_link.vsock.active_path().is_none_or(|p| p.is_relay()) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!host_link.vsock.active_path().unwrap().is_relay());
    exchange(&o.viewer_conn, &o.host.1).await;
    eprintln!(
        "MATRIX | {:<34} | relay → punched {:>5} ms after the firewall opened (re-punch 2 s + 3 s hold); viewer on {viewer_target}",
        "upgrade relay → direct",
        took.as_millis()
    );

    // Now the direct path dies both ways: back to the relay within about a second.
    let closed = Instant::now();
    w.net.set_firewall(ip(VIEWER_PUBLIC), Some(server_ips()));
    w.net.set_firewall(ip(HOST_PUBLIC), Some(server_ips()));
    let failed_over = tokio::time::timeout(
        Duration::from_secs(5),
        watch.wait_for(|s| s.active.is_some_and(|a| a.class == PathClass::Relay)),
    )
    .await
    .is_ok();
    assert!(failed_over, "no failover: {:?}", o.dialed.link.state());
    let took = closed.elapsed();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !host_link.vsock.active_path().is_some_and(|p| p.is_relay()) && Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(host_link.vsock.active_path().unwrap().is_relay());
    exchange(&o.viewer_conn, &o.host.1).await;
    eprintln!(
        "MATRIX | {:<34} | punched → relay {:>5} ms after the direct path was cut",
        "failover direct → relay",
        took.as_millis()
    );
    traffic.abort();
    std::mem::forget(host);
}

/// A paired device's ID now answers with another key: refused before any packet.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dialing_a_paired_id_that_answers_with_another_key_is_refused() {
    let w = world(false).await;
    w.net.add_nat(
        Nat::Easy.config(ip(VIEWER_PUBLIC), 10).unwrap(),
        &[ip(VIEWER_LAN)],
    );
    w.net.add_nat(
        Nat::Easy.config(ip(HOST_PUBLIC), 15).unwrap(),
        &[ip(HOST_LAN)],
    );
    let (viewer, host_dev) = (device(), device());
    let host = start_host(&w, &host_dev, options(&w.net, ip(HOST_LAN))).await;
    let someone_else = DeviceIdentity::generate().public_key();
    let r = rendezvous::dial(
        &w.signal,
        &viewer.keys,
        &host.id,
        Some(someone_else),
        options(&w.net, ip(VIEWER_LAN)),
        &|_: Stage| {},
    )
    .await;
    assert!(
        matches!(r, Err(rendezvous::DialError::WrongDevice)),
        "{:?}",
        r.err()
    );
    let r = rendezvous::dial(
        &w.signal,
        &viewer.keys,
        "100000000",
        None,
        options(&w.net, ip(VIEWER_LAN)),
        &|_: Stage| {},
    )
    .await;
    assert!(
        matches!(r, Err(rendezvous::DialError::Offline)),
        "{:?}",
        r.err()
    );
    std::mem::forget(host);
}
