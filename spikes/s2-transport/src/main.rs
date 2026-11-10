//! M0 S2: validates DETAILED_DESIGN §4.1/§4.8 against quinn 0.11.
//!
//! Usage: s2-transport [scenario-name-filter]

mod media;
mod netem;
mod vsock;

use std::{
    any::Any,
    net::{SocketAddr, UdpSocket},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering::Relaxed},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::Result;
use media::{RxStats, SendTimes, TxStats, VideoCfg};
use netem::{Loss, Netem, NetemCfg};
use quinn::{
    AckFrequencyConfig, ClientConfig, Connection, Endpoint, EndpointConfig, ServerConfig, TokioRuntime, TransportConfig, VarInt,
    congestion::{Controller, ControllerFactory},
    crypto::rustls::{QuicClientConfig, QuicServerConfig},
};
use rustls::{
    DigitallySignedStruct, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime},
};
use tokio::runtime::Runtime;
use vsock::{VIRTUAL_PEER, VirtualSocket};

static START: OnceLock<Instant> = OnceLock::new();

pub fn now_us() -> u64 {
    START.get_or_init(Instant::now).elapsed().as_micros() as u64
}

pub struct Rng(u64);
impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    pub fn f64(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

#[cfg(target_os = "macos")]
pub fn set_realtime_qos() {
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0);
    }
}
#[cfg(not(target_os = "macos"))]
pub fn set_realtime_qos() {}

pub fn wait_until(t: Instant) {
    loop {
        let now = Instant::now();
        if now >= t {
            return;
        }
        let left = t - now;
        if left > Duration::from_millis(2) {
            thread::sleep(left - Duration::from_millis(1));
        } else {
            std::hint::spin_loop();
        }
    }
}

pub fn bind_udp() -> Result<UdpSocket> {
    use socket2::{Domain, Socket, Type};
    let s = Socket::new(Domain::IPV4, Type::DGRAM, None)?;
    s.set_recv_buffer_size(6 << 20)?;
    s.set_send_buffer_size(6 << 20)?;
    s.bind(&"127.0.0.1:0".parse::<SocketAddr>()?.into())?;
    Ok(s.into())
}

/// §4.8: window above u32::MAX switches off both quinn's congestion blocking and its pacer.
#[derive(Clone)]
struct MediaController;

impl Controller for MediaController {
    fn on_congestion_event(&mut self, _: Instant, _: Instant, _: bool, _: u64) {}
    fn on_mtu_update(&mut self, _: u16) {}
    fn window(&self) -> u64 {
        1 << 40
    }
    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(self.clone())
    }
    fn initial_window(&self) -> u64 {
        1 << 40
    }
    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

struct MediaFactory;
impl ControllerFactory for MediaFactory {
    fn build(self: Arc<Self>, _: Instant, _: u16) -> Box<dyn Controller> {
        Box::new(MediaController)
    }
}

fn transport(default_cc: bool) -> Arc<TransportConfig> {
    let mut t = TransportConfig::default();
    if !default_cc {
        t.congestion_controller_factory(Arc::new(MediaFactory));
    }
    t.initial_mtu(1200)
        .min_mtu(1200)
        .mtu_discovery_config(None)
        .datagram_send_buffer_size(512 * 1024)
        .datagram_receive_buffer_size(Some(4 << 20))
        .max_idle_timeout(Some(Duration::from_secs(10).try_into().unwrap()))
        .keep_alive_interval(Some(Duration::from_secs(2)))
        .enable_segmentation_offload(true);
    let mut ack = AckFrequencyConfig::default();
    ack.ack_eliciting_threshold(VarInt::from_u32(16)).max_ack_delay(Some(Duration::from_millis(10)));
    t.ack_frequency_config(Some(ack));
    Arc::new(t)
}

#[derive(Debug)]
struct AnyCert;
impl ServerCertVerifier for AnyCert {
    fn verify_server_cert(&self, _: &CertificateDer<'_>, _: &[CertificateDer<'_>], _: &ServerName<'_>, _: &[u8], _: UnixTime) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519, SignatureScheme::ECDSA_NISTP256_SHA256]
    }
}

fn tls(default_cc: bool) -> Result<(ClientConfig, ServerConfig)> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let c = rcgen::generate_simple_self_signed(vec!["nexa".to_string()])?;
    let cert = CertificateDer::from(c.cert.der().to_vec());
    let key = PrivateKeyDer::try_from(c.signing_key.serialize_der()).map_err(|e| anyhow::anyhow!(e))?;

    let mut sc = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)?;
    sc.alpn_protocols = vec![b"nexa/1".to_vec()];
    sc.max_early_data_size = 0;
    let mut server = ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(sc)?));
    server.transport_config(transport(default_cc));
    server.migration(false);

    let mut cc = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyCert))
        .with_no_client_auth();
    cc.alpn_protocols = vec![b"nexa/1".to_vec()];
    let mut client = ClientConfig::new(Arc::new(QuicClientConfig::try_from(cc)?));
    client.transport_config(transport(default_cc));
    Ok((client, server))
}

fn net_runtime(name: &'static str) -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name(name)
        .on_thread_start(set_realtime_qos)
        .enable_all()
        .build()
        .unwrap()
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum RxMode {
    /// read_datagram on its own OS thread (the design).
    Dedicated,
    /// read_datagram as a task on the tokio net runtime.
    SharedRuntime,
}

#[derive(Clone, Copy, Debug)]
enum Action {
    None,
    PlannedSwitch { at: Duration },
    Failover { at: Duration, detect: Duration },
}

#[derive(Clone)]
struct Scenario {
    name: &'static str,
    paths: Vec<Option<NetemCfg>>,
    video: VideoCfg,
    rx: RxMode,
    action: Action,
}

#[derive(Debug)]
struct Report {
    name: &'static str,
    tx: TxStats,
    rx: RxStats,
    ctrl_host_to_viewer_us: Vec<u32>,
    input_viewer_to_host_us: Vec<u32>,
    max_udp_on_wire: usize,
    exporter_match: bool,
    /// Longest gap between completed frames after the action fired, and frames lost.
    outage_ms: Option<f64>,
    action_at_us: Option<u64>,
    switched_at_us: Option<u64>,
}

fn pct(v: &mut [u32], p: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_unstable();
    v[((v.len() - 1) as f64 * p) as usize] as f64 / 1000.0
}

async fn reader(mut r: quinn::RecvStream, sink: Arc<std::sync::Mutex<Vec<u32>>>) {
    let mut b = [0u8; 16];
    while r.read_exact(&mut b).await.is_ok() {
        let t = u64::from_le_bytes(b[8..16].try_into().unwrap());
        let d = now_us().saturating_sub(t) as u32;
        sink.lock().unwrap().push(d);
    }
}

async fn writer(mut w: quinn::SendStream, period: Duration, stop: Arc<AtomicBool>) {
    let _ = w.set_priority(10);
    let mut tick = tokio::time::interval(period);
    let mut seq = 0u64;
    while !stop.load(Relaxed) {
        tick.tick().await;
        let mut b = [0u8; 16];
        b[..8].copy_from_slice(&seq.to_le_bytes());
        b[8..16].copy_from_slice(&now_us().to_le_bytes());
        if w.write_all(&b).await.is_err() {
            break;
        }
        seq += 1;
    }
}

fn run(s: Scenario) -> Result<Report> {
    let view_rt = net_runtime("viewer-net");
    let host_rt = net_runtime("host-net");

    let viewer_udp = bind_udp()?;
    let host_udp = bind_udp()?;
    let (viewer_addr, host_addr) = (viewer_udp.local_addr()?, host_udp.local_addr()?);

    let mut netems = Vec::new();
    let (mut v_targets, mut h_targets) = (Vec::new(), Vec::new());
    for (i, p) in s.paths.iter().enumerate() {
        match p {
            None => {
                v_targets.push(host_addr);
                h_targets.push(viewer_addr);
            }
            Some(cfg) => {
                let n = Netem::start(*cfg, viewer_addr, host_addr, 1000 + i as u64)?;
                v_targets.push(n.a);
                h_targets.push(n.b);
                netems.push(n);
            }
        }
    }

    let (client_cfg, server_cfg) = tls(s.video.default_cc)?;
    let runtime = Arc::new(TokioRuntime);
    use quinn::Runtime as _;
    let host_vs = {
        let _g = host_rt.enter();
        Arc::new(VirtualSocket::new(runtime.wrap_udp_socket(host_udp)?, h_targets))
    };
    let view_vs = {
        let _g = view_rt.enter();
        Arc::new(VirtualSocket::new(runtime.wrap_udp_socket(viewer_udp)?, v_targets))
    };

    let host_ep = {
        let _g = host_rt.enter();
        Endpoint::new_with_abstract_socket(EndpointConfig::default(), Some(server_cfg), host_vs.clone(), runtime.clone())?
    };
    let view_ep = {
        let _g = view_rt.enter();
        let mut e = Endpoint::new_with_abstract_socket(EndpointConfig::default(), None, view_vs.clone(), runtime.clone())?;
        e.set_default_client_config(client_cfg);
        e
    };

    let host_conn_task = host_rt.spawn({
        let ep = host_ep.clone();
        async move { ep.accept().await.unwrap().await.unwrap() }
    });
    let view_conn: Connection = view_rt.block_on(async { view_ep.connect(VIRTUAL_PEER, "nexa")?.await.map_err(anyhow::Error::from) })?;
    let host_conn: Connection = host_rt.block_on(host_conn_task)?;

    // §9.2: both ends must derive identical exporter material.
    let (mut ea, mut eb) = ([0u8; 32], [0u8; 32]);
    view_conn.export_keying_material(&mut ea, b"EXPORTER-nexa-pair", b"").map_err(|_| anyhow::anyhow!("exporter"))?;
    host_conn.export_keying_material(&mut eb, b"EXPORTER-nexa-pair", b"").map_err(|_| anyhow::anyhow!("exporter"))?;

    let stop = Arc::new(AtomicBool::new(false));
    let ctrl = Arc::new(std::sync::Mutex::new(Vec::<u32>::new()));
    let input = Arc::new(std::sync::Mutex::new(Vec::<u32>::new()));

    // Control (host→viewer, 200 Hz) on a bidi stream the viewer opens.
    let (mut vs, vr) = view_rt.block_on(view_conn.open_bi())?;
    view_rt.block_on(vs.write_all(&[1]))?;
    view_rt.spawn(reader(vr, ctrl.clone()));
    let (hs, mut hr) = host_rt.block_on(host_conn.accept_bi())?;
    let mut ty = [0u8; 1];
    host_rt.block_on(hr.read_exact(&mut ty))?;
    host_rt.spawn(writer(hs, Duration::from_millis(5), stop.clone()));

    // Input (viewer→host, 1 kHz) on a uni stream.
    let mut input_w = view_rt.block_on(view_conn.open_uni())?;
    view_rt.block_on(input_w.write_all(&[2]))?;
    let mut input_r = host_rt.block_on(host_conn.accept_uni())?;
    host_rt.block_on(input_r.read_exact(&mut ty))?;
    host_rt.spawn(reader(input_r, input.clone()));
    view_rt.spawn(writer(input_w, Duration::from_millis(1), stop.clone()));

    // PathSwitch control (viewer→host), only ever trusted over this authenticated stream.
    let mut path_w = view_rt.block_on(view_conn.open_uni())?;
    view_rt.block_on(path_w.write_all(&[9]))?;
    let mut path_r = host_rt.block_on(host_conn.accept_uni())?;
    host_rt.block_on(path_r.read_exact(&mut ty))?;
    {
        let vs = host_vs.clone();
        host_rt.spawn(async move {
            let mut b = [0u8; 1];
            while path_r.read_exact(&mut b).await.is_ok() {
                vs.set_active(b[0] as usize);
            }
        });
    }

    let times = Arc::new(SendTimes::new());
    let rx_join: Box<dyn FnOnce() -> RxStats + Send> = match s.rx {
        RxMode::Dedicated => {
            let (c, t) = (view_conn.clone(), times.clone());
            let h = thread::Builder::new().name("media-rx".into()).spawn(move || {
                set_realtime_qos();
                tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap().block_on(media::run_receiver(c, t))
            })?;
            Box::new(move || h.join().unwrap())
        }
        RxMode::SharedRuntime => {
            let h = view_rt.spawn(media::run_receiver(view_conn.clone(), times.clone()));
            let handle = view_rt.handle().clone();
            Box::new(move || handle.block_on(h).unwrap())
        }
    };

    let tx_thread = {
        let (c, v, t, st) = (host_conn.clone(), s.video, times.clone(), stop.clone());
        thread::Builder::new().name("media-tx".into()).spawn(move || media::run_sender(c, v, t, st))?
    };

    let (mut action_at, mut switched_at) = (None, None);
    match s.action {
        Action::None => {}
        Action::PlannedSwitch { at } => {
            thread::sleep(at);
            action_at = Some(now_us());
            view_vs.set_active(1);
            view_rt.block_on(path_w.write_all(&[1]))?;
            switched_at = action_at;
        }
        Action::Failover { at, detect } => {
            thread::sleep(at);
            action_at = Some(now_us());
            netems[0].ctl.blackhole.store(true, Relaxed);
            loop {
                let silent = now_us().saturating_sub(view_vs.last_rx_us.load(Relaxed));
                if silent >= detect.as_micros() as u64 {
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
            view_vs.set_active(1);
            view_rt.block_on(path_w.write_all(&[1]))?;
            switched_at = Some(now_us());
        }
    }

    let tx = tx_thread.join().unwrap();
    thread::sleep(Duration::from_millis(300));
    stop.store(true, Relaxed);
    let max_udp_on_wire = netems.iter().map(|n| n.ctl.max_udp_len.load(Relaxed)).max().unwrap_or(0);

    view_conn.close(0u32.into(), b"done");
    let rx = rx_join();
    host_ep.close(0u32.into(), b"done");
    for n in netems {
        n.stop();
    }

    let outage_ms = action_at.map(|t| {
        let mut prev = t;
        let mut gap = 0;
        for (done, _) in rx.frames.iter().filter(|(d, _)| *d >= t) {
            gap = gap.max(done - prev);
            prev = *done;
        }
        gap as f64 / 1000.0
    });

    let ctrl_host_to_viewer_us = std::mem::take(&mut *ctrl.lock().unwrap());
    let input_viewer_to_host_us = std::mem::take(&mut *input.lock().unwrap());
    view_rt.shutdown_background();
    host_rt.shutdown_background();

    Ok(Report {
        name: s.name,
        tx,
        rx,
        ctrl_host_to_viewer_us,
        input_viewer_to_host_us,
        max_udp_on_wire,
        exporter_match: ea == eb,
        outage_ms,
        action_at_us: action_at,
        switched_at_us: switched_at,
    })
}

/// Raw-UDP reference for "added by quinn": same packet schedule, no QUIC.
fn raw_baseline(secs: f64, fps: f64, mbps: f64) -> Result<Vec<u32>> {
    let rx = bind_udp()?;
    let tx = bind_udp()?;
    let dest = rx.local_addr()?;
    rx.set_read_timeout(Some(Duration::from_millis(500)))?;
    let h = thread::spawn(move || {
        set_realtime_qos();
        let mut v = Vec::with_capacity(1 << 20);
        let mut b = [0u8; 2048];
        while let Ok((_, _)) = rx.recv_from(&mut b) {
            let t = u64::from_le_bytes(b[..8].try_into().unwrap());
            v.push(now_us().saturating_sub(t) as u32);
        }
        v
    });
    set_realtime_qos();
    let interval = Duration::from_secs_f64(1.0 / fps);
    let pkt = 1162usize;
    let n_per_frame = (mbps * 1e6 / 8.0 / fps / pkt as f64).ceil() as usize;
    let t0 = Instant::now();
    let mut b = vec![0u8; pkt];
    for f in 0..(secs * fps) as u32 {
        wait_until(t0 + interval * f);
        let start = Instant::now();
        let window = interval.mul_f64(0.5).min(Duration::from_secs_f64((n_per_frame * pkt) as f64 * 8.0 / (1.5 * mbps * 1e6)));
        for i in 0..n_per_frame {
            let due = start + window.mul_f64(i as f64 / n_per_frame as f64);
            if due > Instant::now() + Duration::from_micros(500) {
                wait_until(due);
            }
            b[..8].copy_from_slice(&now_us().to_le_bytes());
            let _ = tx.send_to(&b, dest);
        }
    }
    Ok(h.join().unwrap())
}

fn check(pass: &mut bool, label: &str, ok: bool, detail: String) {
    *pass &= ok;
    println!("    [{}] {label}: {detail}", if ok { "PASS" } else { "FAIL" });
}

fn main() -> Result<()> {
    now_us();
    let filter = std::env::args().nth(1).unwrap_or_default();

    let v100 = VideoCfg { secs: 10.0, fps: 120.0, mbps: 100.0, fec: 0.0, big_every: 0, pace_bps: 0.0, default_cc: false };
    let lossy = |l: Loss| Some(NetemCfg { one_way_delay: Duration::from_millis(10), loss: l });
    let clean = Some(NetemCfg { one_way_delay: Duration::from_millis(10), loss: Loss::None });
    let scenarios = vec![
        Scenario { name: "t1-direct-100mbps", paths: vec![None], video: v100, rx: RxMode::Dedicated, action: Action::None },
        Scenario { name: "t1b-rx-on-tokio", paths: vec![None], video: v100, rx: RxMode::SharedRuntime, action: Action::None },
        Scenario { name: "x1-pace-800mbps", paths: vec![None], video: VideoCfg { pace_bps: 800e6, ..v100 }, rx: RxMode::Dedicated, action: Action::None },
        Scenario { name: "x2-cubic-clean", paths: vec![clean], video: VideoCfg { default_cc: true, ..v100 }, rx: RxMode::Dedicated, action: Action::None },
        Scenario { name: "x3-cubic-loss2pct-fec25", paths: vec![lossy(Loss::Random(0.02))], video: VideoCfg { default_cc: true, fec: 0.25, ..v100 }, rx: RxMode::Dedicated, action: Action::None },
        Scenario { name: "t2-rtt20-clean", paths: vec![clean], video: v100, rx: RxMode::Dedicated, action: Action::None },
        Scenario { name: "t3-loss2pct-fec25", paths: vec![lossy(Loss::Random(0.02))], video: VideoCfg { fec: 0.25, ..v100 }, rx: RxMode::Dedicated, action: Action::None },
        Scenario { name: "t3b-loss2pct-nofec", paths: vec![lossy(Loss::Random(0.02))], video: v100, rx: RxMode::Dedicated, action: Action::None },
        Scenario { name: "t3c-burst-fec25", paths: vec![lossy(Loss::Burst { enter: 0.002, exit: 0.1, bad_loss: 0.5 })], video: VideoCfg { fec: 0.25, ..v100 }, rx: RxMode::Dedicated, action: Action::None },
        Scenario { name: "t4a-planned-switch", paths: vec![clean, Some(NetemCfg { one_way_delay: Duration::from_millis(3), loss: Loss::None })], video: VideoCfg { fec: 0.05, ..v100 }, rx: RxMode::Dedicated, action: Action::PlannedSwitch { at: Duration::from_secs(4) } },
        Scenario { name: "t4b-failover", paths: vec![clean, clean], video: VideoCfg { fec: 0.05, ..v100 }, rx: RxMode::Dedicated, action: Action::Failover { at: Duration::from_secs(4), detect: Duration::from_secs(1) } },
        Scenario { name: "t5-stress-400mbps", paths: vec![None], video: VideoCfg { secs: 6.0, mbps: 400.0, fec: 0.1, big_every: 60, ..v100 }, rx: RxMode::Dedicated, action: Action::None },
    ];

    let mut all = true;
    let mut raw_p99_us = None;
    if filter.is_empty() || "t1-direct-100mbps".contains(&filter) {
        let mut raw = raw_baseline(5.0, 120.0, 100.0)?;
        let (p50, p99) = (pct(&mut raw, 0.5), pct(&mut raw, 0.99));
        println!("raw-udp baseline: packets={} p50={p50:.3}ms p99={p99:.3}ms", raw.len());
        raw_p99_us = Some(p99);
    }

    for s in scenarios {
        if !filter.is_empty() && !s.name.contains(&filter) {
            continue;
        }
        let name = s.name;
        let netem_delay_ms = s.paths[0].map_or(0.0, |c| c.one_way_delay.as_secs_f64() * 1000.0);
        let mut r = run(s)?;
        let mut flat: Vec<u32> = r.rx.frames.iter().map(|f| f.1).collect();
        let complete = r.rx.frames.len() as f64 / r.tx.frames.max(1) as f64;
        let (pk50, pk99, pk999) = (pct(&mut r.rx.packet_latency_us, 0.5), pct(&mut r.rx.packet_latency_us, 0.99), pct(&mut r.rx.packet_latency_us, 0.999));
        let (fr50, fr99) = (pct(&mut flat, 0.5), pct(&mut flat, 0.99));
        let (c50, c99) = (pct(&mut r.ctrl_host_to_viewer_us, 0.5), pct(&mut r.ctrl_host_to_viewer_us, 0.99));
        let (i50, i99) = (pct(&mut r.input_viewer_to_host_us, 0.5), pct(&mut r.input_viewer_to_host_us, 0.99));
        let actual_mbps = r.tx.bytes as f64 * 8.0 / 1e6 / (r.tx.frames.max(1) as f64 / 120.0);
        println!("\n== {name}");
        println!(
            "  tx: frames={} skipped={} packets={} rate={actual_mbps:.0}Mbps shard={} max_dg={} buffer_waits={} fec_cpu={:.1}ms total",
            r.tx.frames, r.tx.skipped, r.tx.packets, r.tx.shard_len, r.tx.max_datagram, r.tx.buffer_waits, r.tx.fec_encode_us as f64 / 1000.0
        );
        println!("  packet one-way: p50={pk50:.3} p99={pk99:.3} p99.9={pk999:.3} ms (jitter p99-p50={:.3})", pk99 - pk50);
        println!("  frame capture→complete: p50={fr50:.3} p99={fr99:.3} ms, complete={:.3}% recovered_shards={} corrupt={}", complete * 100.0, r.rx.fec_recovered, r.rx.corrupt);
        println!("  control(h→v): p50={c50:.3} p99={c99:.3} ms n={}", r.ctrl_host_to_viewer_us.len());
        println!("  input(v→h):   p50={i50:.3} p99={i99:.3} ms n={}", r.input_viewer_to_host_us.len());
        if r.max_udp_on_wire > 0 {
            println!("  max UDP on wire: {} bytes", r.max_udp_on_wire);
        }
        if let (Some(g), Some(a), Some(sw)) = (r.outage_ms, r.action_at_us, r.switched_at_us) {
            println!("  longest gap after action: {g:.1} ms (switch {:.1} ms after event)", (sw - a) as f64 / 1000.0);
        }

        let mut p = true;
        check(&mut p, "no corrupt frames", r.rx.corrupt == 0, format!("{}", r.rx.corrupt));
        check(&mut p, "exporter matches", r.exporter_match, String::new());
        match name {
            "t1-direct-100mbps" => {
                let base = raw_p99_us.unwrap_or(0.0);
                check(&mut p, "quinn-added p99 < 1 ms", pk99 - base < 1.0, format!("{:.3} ms over raw", pk99 - base));
                check(&mut p, "not blocked by quinn", r.tx.buffer_waits == 0 && r.tx.skipped == 0, format!("waits={} skipped={}", r.tx.buffer_waits, r.tx.skipped));
                check(&mut p, "all frames delivered", complete >= 0.999, format!("{:.3}%", complete * 100.0));
                check(&mut p, "control p99 <= RTT+2ms", c99 <= 2.0, format!("{c99:.3} ms"));
                check(&mut p, "rx jitter p99-p50 <= 1ms", pk99 - pk50 <= 1.0, format!("{:.3} ms", pk99 - pk50));
                check(&mut p, "max_datagram consistent with 1200 MTU", r.tx.max_datagram <= 1200 && r.rx.max_payload == r.tx.shard_len + media::HDR, format!("{} / payload {}", r.tx.max_datagram, r.rx.max_payload));
            }
            "t1b-rx-on-tokio" => check(&mut p, "rx jitter p99-p50 <= 1ms", pk99 - pk50 <= 1.0, format!("{:.3} ms", pk99 - pk50)),
            "t2-rtt20-clean" => {
                check(&mut p, "control p99 <= one-way+2ms", c99 <= netem_delay_ms + 2.0, format!("{c99:.3} ms"));
                check(&mut p, "wire size <= 1200", r.max_udp_on_wire <= 1200, format!("{}", r.max_udp_on_wire));
            }
            "t3-loss2pct-fec25" => check(&mut p, "complete > 99.9%", complete > 0.999, format!("{:.3}%", complete * 100.0)),
            "t3c-burst-fec25" => check(&mut p, "complete > 99%", complete > 0.99, format!("{:.3}%", complete * 100.0)),
            "t4a-planned-switch" => {
                let g = r.outage_ms.unwrap();
                check(&mut p, "gap < 100 ms", g < 100.0, format!("{g:.1} ms"));
                check(&mut p, "connection survived", complete > 0.99, format!("{:.3}%", complete * 100.0));
            }
            "t4b-failover" => {
                let g = r.outage_ms.unwrap();
                check(&mut p, "recovers within 1.5 s", g < 1500.0, format!("{g:.1} ms"));
            }
            "t5-stress-400mbps" => check(&mut p, "sustains target", r.tx.skipped == 0 && complete > 0.99, format!("skipped={} complete={:.3}%", r.tx.skipped, complete * 100.0)),
            _ => {}
        }
        all &= p;
    }
    println!("\nOVERALL: {}", if all { "PASS" } else { "FAIL" });
    Ok(())
}
