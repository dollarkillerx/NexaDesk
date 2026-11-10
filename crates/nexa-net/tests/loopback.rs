//! The production socket, QUIC config, packetizer and assembler over a real QUIC connection.

use std::{
    net::{SocketAddr, UdpSocket},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering::Relaxed},
    },
    thread,
    time::{Duration, Instant},
};

use nexa_net::{
    cc::transport_config,
    shard::{Assembler, AssemblerEvent, FrameInfo, Packetizer},
    vsock::{VIRTUAL_PEER, VirtualSocket, bind_media_socket},
};
use nexa_wire::datagram::VideoShardHeader;
use quinn::{
    ClientConfig, Connection, Endpoint, EndpointConfig, Runtime, ServerConfig, TokioRuntime,
    crypto::rustls::{QuicClientConfig, QuicServerConfig},
};
use rustls::{
    DigitallySignedStruct, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime},
};

fn clock() -> Box<dyn Fn() -> u64 + Send + Sync> {
    static START: OnceLock<Instant> = OnceLock::new();
    let s = *START.get_or_init(Instant::now);
    Box::new(move || s.elapsed().as_micros() as u64)
}

#[derive(Debug)]
struct AnyCert;
impl ServerCertVerifier for AnyCert {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::ED25519,
            SignatureScheme::ECDSA_NISTP256_SHA256,
        ]
    }
}

fn tls() -> (ClientConfig, ServerConfig) {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let c = rcgen::generate_simple_self_signed(vec!["nexa".to_string()]).unwrap();
    let cert = CertificateDer::from(c.cert.der().to_vec());
    let key = PrivateKeyDer::try_from(c.signing_key.serialize_der()).unwrap();
    let mut sc = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .unwrap();
    sc.alpn_protocols = vec![b"nexa/1".to_vec()];
    let mut server = ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(sc).unwrap()));
    server.transport_config(transport_config());
    server.migration(false);
    let mut cc = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyCert))
        .with_no_client_auth();
    cc.alpn_protocols = vec![b"nexa/1".to_vec()];
    let mut client = ClientConfig::new(Arc::new(QuicClientConfig::try_from(cc).unwrap()));
    client.transport_config(transport_config());
    (client, server)
}

struct Pair {
    viewer: Connection,
    host: Connection,
    viewer_vs: Arc<VirtualSocket>,
    host_vs: Arc<VirtualSocket>,
    viewer_addr: SocketAddr,
    host_addr: SocketAddr,
    stun_seen: Arc<Mutex<Vec<Vec<u8>>>>,
    _eps: (Endpoint, Endpoint),
}

async fn connect() -> Pair {
    let any: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let vu = bind_media_socket(any).unwrap();
    let hu = bind_media_socket(any).unwrap();
    let (viewer_addr, host_addr) = (vu.local_addr().unwrap(), hu.local_addr().unwrap());
    let rt = Arc::new(TokioRuntime);
    let stun_seen = Arc::new(Mutex::new(Vec::new()));
    let sink = stun_seen.clone();
    let viewer_vs = Arc::new(VirtualSocket::new(
        rt.wrap_udp_socket(vu).unwrap(),
        host_addr,
        Box::new(move |b, _| sink.lock().unwrap().push(b.to_vec())),
        clock(),
    ));
    let host_vs = Arc::new(VirtualSocket::new(
        rt.wrap_udp_socket(hu).unwrap(),
        viewer_addr,
        Box::new(|_, _| {}),
        clock(),
    ));
    let (cc, sc) = tls();
    let host_ep = Endpoint::new_with_abstract_socket(
        EndpointConfig::default(),
        Some(sc),
        host_vs.clone(),
        rt.clone(),
    )
    .unwrap();
    let mut view_ep =
        Endpoint::new_with_abstract_socket(EndpointConfig::default(), None, viewer_vs.clone(), rt)
            .unwrap();
    view_ep.set_default_client_config(cc);
    let accept = tokio::spawn({
        let e = host_ep.clone();
        async move { e.accept().await.unwrap().await.unwrap() }
    });
    let viewer = view_ep
        .connect(VIRTUAL_PEER, "nexa")
        .unwrap()
        .await
        .unwrap();
    let host = accept.await.unwrap();
    Pair {
        viewer,
        host,
        viewer_vs,
        host_vs,
        viewer_addr,
        host_addr,
        stun_seen,
        _eps: (view_ep, host_ep),
    }
}

fn assembler() -> Assembler {
    Assembler::new(Duration::from_millis(200), Duration::from_millis(50))
}

async fn send_and_collect(
    p: &mut Packetizer,
    asm: &mut Assembler,
    host: &Connection,
    viewer: &Connection,
    first: u32,
    frames: u32,
    len: usize,
) -> Vec<Vec<u8>> {
    let mut got = Vec::new();
    let mut ev = Vec::new();
    for n in 0..frames {
        let body: Vec<u8> = (0..len).map(|i| (i as u32 + n) as u8).collect();
        let info = FrameInfo {
            stream: 0,
            flags: 0,
            ref_epoch: 0,
            frame_id: first + n,
            capture_ts_us: n,
        };
        for pkt in p
            .packetize(&body, info, 0.1, host.max_datagram_size().unwrap())
            .unwrap()
        {
            while host.datagram_send_buffer_space() < pkt.len() {
                tokio::time::sleep(Duration::from_micros(200)).await;
            }
            host.send_datagram(pkt).unwrap();
        }
        loop {
            ev.retain(|e: &AssemblerEvent| {
                if let AssemblerEvent::Complete(f) = e {
                    got.push(f.data.clone());
                }
                false
            });
            if got.len() as u32 > n {
                break;
            }
            let d = match tokio::time::timeout(Duration::from_secs(5), viewer.read_datagram()).await
            {
                Ok(d) => d.unwrap(),
                Err(_) => {
                    let (hs, vs) = (host.stats(), viewer.stats());
                    panic!(
                        "timeout at frame {n}: got {} tx_datagrams={} rx_datagrams={} tx_udp={} rx_udp={} lost_packets={}",
                        got.len(),
                        hs.frame_tx.datagram,
                        vs.frame_rx.datagram,
                        hs.udp_tx.datagrams,
                        vs.udp_rx.datagrams,
                        hs.path.lost_packets
                    );
                }
            };
            let (h, body) = VideoShardHeader::parse(&d).unwrap();
            asm.push(h, body, Instant::now(), &mut ev);
        }
    }
    got
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn frames_arrive_exactly() {
    let c = connect().await;
    let (mut p, mut a) = (Packetizer::default(), assembler());
    let got = send_and_collect(&mut p, &mut a, &c.host, &c.viewer, 0, 50, 40_000).await;
    assert_eq!(got.len(), 50);
    for (n, f) in got.iter().enumerate() {
        assert!(
            f.iter()
                .enumerate()
                .all(|(i, b)| *b == (i as u32 + n as u32) as u8)
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stun_is_diverted_and_strangers_are_dropped() {
    let c = connect().await;
    let stranger = UdpSocket::bind("127.0.0.1:0").unwrap();

    let stun = [
        0x00u8, 0x01, 0x00, 0x00, 0x21, 0x12, 0xA4, 0x42, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12,
    ];
    stranger.send_to(&stun, c.viewer_addr).unwrap();
    stranger.send_to(&[0x40u8; 64], c.viewer_addr).unwrap();

    let deadline = Instant::now() + Duration::from_secs(3);
    while c.viewer_vs.dropped_unvalidated() == 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(c.stun_seen.lock().unwrap().as_slice(), &[stun.to_vec()]);
    assert_eq!(c.viewer_vs.dropped_unvalidated(), 1);

    // The connection is unaffected.
    let (mut p, mut a) = (Packetizer::default(), assembler());
    assert_eq!(
        send_and_collect(&mut p, &mut a, &c.host, &c.viewer, 0, 5, 3_000)
            .await
            .len(),
        5
    );
}

/// A dumb UDP relay standing in for a second path.
fn forwarder(
    viewer: SocketAddr,
    host: SocketAddr,
    stop: Arc<AtomicBool>,
) -> (SocketAddr, SocketAddr) {
    let any: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let a = bind_media_socket(any).unwrap();
    let b = bind_media_socket(any).unwrap();
    let addrs = (a.local_addr().unwrap(), b.local_addr().unwrap());
    for (rx, tx, dest) in [
        (a.try_clone().unwrap(), b.try_clone().unwrap(), host),
        (b, a, viewer),
    ] {
        let stop = stop.clone();
        thread::spawn(move || {
            rx.set_read_timeout(Some(Duration::from_millis(20)))
                .unwrap();
            let mut buf = [0u8; 2048];
            while !stop.load(Relaxed) {
                if let Ok((n, _)) = rx.recv_from(&mut buf) {
                    let _ = tx.send_to(&buf[..n], dest);
                }
            }
        });
    }
    addrs
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection_survives_path_switch() {
    let c = connect().await;
    let stop = Arc::new(AtomicBool::new(false));
    let (mut p, mut a) = (Packetizer::default(), assembler());
    assert_eq!(
        send_and_collect(&mut p, &mut a, &c.host, &c.viewer, 0, 10, 8_000)
            .await
            .len(),
        10
    );

    let (via_viewer, via_host) = forwarder(c.viewer_addr, c.host_addr, stop.clone());
    c.viewer_vs.switch_to(via_viewer);
    c.host_vs.switch_to(via_host);
    assert_eq!(c.viewer_vs.active(), via_viewer);

    assert_eq!(
        send_and_collect(&mut p, &mut a, &c.host, &c.viewer, 10, 10, 8_000)
            .await
            .len(),
        10
    );
    stop.store(true, Relaxed);
}
