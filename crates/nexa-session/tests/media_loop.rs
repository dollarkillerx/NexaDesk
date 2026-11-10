//! The whole media path over real QUIC with fake codecs. The fake decoder fails the
//! test if a frame arrives whose reference it never decoded, the bug that real
//! hardware decoders would hide (M0: garbage picture, no error).

use std::{
    net::{SocketAddr, UdpSocket},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed},
    },
    thread,
    time::{Duration, Instant},
};

use nexa_media::*;
use nexa_net::{
    cc::transport_config,
    rate::RateConfig,
    vsock::{VIRTUAL_PEER, VirtualSocket, bind_media_socket},
};
use nexa_session::{
    host::{HostMedia, HostParams},
    viewer::{ViewerMedia, ViewerParams},
};
use nexa_wire::control::Mode;
use quinn::{
    ClientConfig, Connection, Endpoint, EndpointConfig, Runtime, ServerConfig, TokioRuntime,
    crypto::rustls::{QuicClientConfig, QuicServerConfig},
};
use rustls::{
    DigitallySignedStruct, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime},
};

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
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
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

/// Forwards viewer↔host, dropping host→viewer packets while `blackout` is set.
fn proxy(
    viewer: SocketAddr,
    host: SocketAddr,
    blackout: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) -> (SocketAddr, SocketAddr) {
    let any: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let (a, b) = (
        bind_media_socket(any).unwrap(),
        bind_media_socket(any).unwrap(),
    );
    let addrs = (a.local_addr().unwrap(), b.local_addr().unwrap());
    for (rx, tx, dest, lossy) in [
        (a.try_clone().unwrap(), b.try_clone().unwrap(), host, false),
        (b, a, viewer, true),
    ] {
        let (stop, blackout) = (stop.clone(), blackout.clone());
        thread::spawn(move || {
            rx.set_read_timeout(Some(Duration::from_millis(20)))
                .unwrap();
            let mut buf = [0u8; 2048];
            while !stop.load(Relaxed) {
                if let Ok((n, _)) = rx.recv_from(&mut buf)
                    && !(lossy && blackout.load(Relaxed))
                {
                    let _ = tx.send_to(&buf[..n], dest);
                }
            }
        });
    }
    addrs
}

struct Net {
    host: Connection,
    viewer: Connection,
    _eps: (Endpoint, Endpoint),
}

async fn connect(blackout: Arc<AtomicBool>, stop: Arc<AtomicBool>) -> Net {
    let any: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let (vu, hu): (UdpSocket, UdpSocket) = (
        bind_media_socket(any).unwrap(),
        bind_media_socket(any).unwrap(),
    );
    let (va, ha) = (vu.local_addr().unwrap(), hu.local_addr().unwrap());
    let (to_host, to_viewer) = proxy(va, ha, blackout, stop);
    let rt = Arc::new(TokioRuntime);
    let clock = || -> Box<dyn Fn() -> u64 + Send + Sync> {
        let s = Instant::now();
        Box::new(move || s.elapsed().as_micros() as u64)
    };
    let v = Arc::new(VirtualSocket::new(
        rt.wrap_udp_socket(vu).unwrap(),
        to_host,
        Box::new(|_, _| {}),
        clock(),
    ));
    let h = Arc::new(VirtualSocket::new(
        rt.wrap_udp_socket(hu).unwrap(),
        to_viewer,
        Box::new(|_, _| {}),
        clock(),
    ));
    let (cc, sc) = tls();
    let he = Endpoint::new_with_abstract_socket(EndpointConfig::default(), Some(sc), h, rt.clone())
        .unwrap();
    let mut ve =
        Endpoint::new_with_abstract_socket(EndpointConfig::default(), None, v, rt).unwrap();
    ve.set_default_client_config(cc);
    let accept = tokio::spawn({
        let e = he.clone();
        async move { e.accept().await.unwrap().await.unwrap() }
    });
    let viewer = ve.connect(VIRTUAL_PEER, "nexa").unwrap().await.unwrap();
    let host = accept.await.unwrap();
    Net {
        host,
        viewer,
        _eps: (ve, he),
    }
}

fn image() -> NativeImage {
    NativeImage::Cpu(Arc::new(CpuFrame {
        width: 1,
        height: 1,
        y: vec![0],
        uv: vec![0],
    }))
}

/// Delivers `frames` frames paced at `interval`, then only Idle.
struct FakeCapturer {
    left: u32,
    interval: Duration,
    next_at: Instant,
}

impl Capturer for FakeCapturer {
    fn next(&mut self, timeout: Duration) -> Result<CaptureEvent> {
        if self.left == 0 {
            thread::sleep(timeout);
            return Ok(CaptureEvent::Idle);
        }
        let wait = self.next_at.saturating_duration_since(Instant::now());
        thread::sleep(wait);
        self.next_at += self.interval;
        self.left -= 1;
        Ok(CaptureEvent::Frame(CapturedFrame {
            image: image(),
            captured_at: Instant::now(),
            dirty: vec![],
        }))
    }
}

#[derive(Default)]
struct Shared {
    invalidations: AtomicU32,
    acks: AtomicU32,
    keys: AtomicU32,
    ref_errors: AtomicU32,
    last_presented: AtomicU64,
    decoded: Mutex<Vec<u32>>,
}

/// Payload: `[frame_id, reference_id]`, padded to look like a real frame.
struct FakeEncoder {
    s: Arc<Shared>,
    last: Option<u32>,
    override_ref: Option<Option<u32>>,
    size: usize,
}

impl Encoder for FakeEncoder {
    fn caps(&self) -> EncoderCaps {
        EncoderCaps {
            ltr: true,
            ..Default::default()
        }
    }
    fn encode(
        &mut self,
        _: &CapturedFrame,
        id: FrameId,
        o: EncodeOpts,
        out: &mut Vec<u8>,
    ) -> Result<EncodedMeta> {
        let reference = if o.force_key {
            None
        } else {
            self.override_ref.take().unwrap_or(self.last)
        };
        out.extend_from_slice(&id.to_le_bytes());
        out.extend_from_slice(&reference.unwrap_or(u32::MAX).to_le_bytes());
        out.resize(self.size, 7);
        self.last = Some(id);
        if reference.is_none() {
            self.s.keys.fetch_add(1, Relaxed);
        }
        Ok(EncodedMeta {
            frame_id: id,
            key: reference.is_none(),
            recovery_point: false,
        })
    }
    fn set_bitrate(&mut self, _: u32) -> Result<()> {
        Ok(())
    }
    fn invalidate_from(&mut self, first_lost: FrameId) -> Result<Recovery> {
        self.s.invalidations.fetch_add(1, Relaxed);
        self.override_ref = Some(first_lost.checked_sub(1));
        Ok(Recovery::RefInvalidated)
    }
    fn on_frame_acked(&mut self, _: FrameId) {
        self.s.acks.fetch_add(1, Relaxed);
    }
    fn force_idr(&mut self) {}
}

struct FakeDecoder(Arc<Shared>);

impl Decoder for FakeDecoder {
    fn decode(&mut self, au: &[u8], meta: FrameMeta) -> Result<DecodedFrame> {
        let id = u32::from_le_bytes(au[0..4].try_into().unwrap());
        let r = u32::from_le_bytes(au[4..8].try_into().unwrap());
        let mut d = self.0.decoded.lock().unwrap();
        if r != u32::MAX && !d.contains(&r) {
            self.0.ref_errors.fetch_add(1, Relaxed);
        }
        d.push(id);
        Ok(DecodedFrame {
            image: image(),
            meta,
        })
    }
}

struct FakePresenter(Arc<Shared>);

impl Presenter for FakePresenter {
    fn resize(&mut self, _: u32, _: u32, _: f32) -> Result<()> {
        Ok(())
    }
    fn set_mode(&mut self, _: PresentMode) -> Result<()> {
        Ok(())
    }
    fn present(&mut self, f: &DecodedFrame) -> Result<PresentStat> {
        self.0
            .last_presented
            .store(u64::from(f.meta.frame_id), Relaxed);
        Ok(PresentStat::default())
    }
}

struct Rig {
    host: HostMedia,
    viewer: ViewerMedia,
    s: Arc<Shared>,
    blackout: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    _net: Net,
}

async fn rig(frames: u32, fps: u32, size: usize) -> Rig {
    let (blackout, stop) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    let net = connect(blackout.clone(), stop.clone()).await;
    let s = Arc::new(Shared::default());
    let interval = Duration::from_secs_f64(1.0 / f64::from(fps));
    let host = HostMedia::spawn(
        net.host.clone(),
        Box::new(FakeCapturer {
            left: frames,
            interval,
            next_at: Instant::now(),
        }),
        Box::new(FakeEncoder {
            s: s.clone(),
            last: None,
            override_ref: None,
            size,
        }),
        HostParams {
            stream: 0,
            fps,
            mode: Mode::Game,
            rate: RateConfig {
                initial_bps: 20_000_000,
                min_bps: 300_000,
                max_bps: 400_000_000,
            },
        },
        Box::new(|_| {}),
    );
    let viewer = ViewerMedia::spawn(
        net.viewer.clone(),
        Box::new(FakeDecoder(s.clone())),
        Box::new(FakePresenter(s.clone())),
        ViewerParams {
            stream: 0,
            frame_deadline: Duration::from_millis(25),
            reorder_grace: Duration::from_millis(2),
        },
    );
    Rig {
        host,
        viewer,
        s,
        blackout,
        stop,
        _net: net,
    }
}

async fn until(what: &str, max: Duration, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + max;
    while !f() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn finish(r: Rig) {
    r.stop.store(true, Relaxed);
    r.viewer.stop();
    r.host.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn frames_flow_end_to_end() {
    let r = rig(120, 120, 20_000).await;
    let s = r.s.clone();
    // Back-pressure may legitimately skip frames before encoding, so count what was encoded.
    let handled =
        || r.host.stats.encoded.load(Relaxed) + r.host.stats.skipped_no_room.load(Relaxed);
    until("capture finished", Duration::from_secs(10), || {
        handled() >= 120
    })
    .await;
    let encoded = r.host.stats.encoded.load(Relaxed) as usize;
    assert!(encoded >= 100, "encoded only {encoded}");
    until("all frames decoded", Duration::from_secs(10), || {
        s.decoded.lock().unwrap().len() >= encoded
    })
    .await;
    assert_eq!(s.ref_errors.load(Relaxed), 0);
    assert_eq!(s.invalidations.load(Relaxed), 0);
    assert_eq!(s.keys.load(Relaxed), 1);
    assert!(s.acks.load(Relaxed) >= 100, "acks {}", s.acks.load(Relaxed));
    until("last frame presented", Duration::from_secs(2), || {
        s.last_presented.load(Relaxed) + 1 >= encoded as u64
    })
    .await;
    finish(r);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blackout_recovers_without_decoding_broken_frames() {
    let r = rig(400, 120, 20_000).await;
    let s = r.s.clone();
    until("streaming", Duration::from_secs(5), || {
        s.decoded.lock().unwrap().len() >= 30
    })
    .await;
    r.blackout.store(true, Relaxed);
    tokio::time::sleep(Duration::from_millis(60)).await;
    r.blackout.store(false, Relaxed);
    until("recovery", Duration::from_secs(10), || {
        s.invalidations.load(Relaxed) >= 1 && s.last_presented.load(Relaxed) >= 380
    })
    .await;
    assert_eq!(
        s.ref_errors.load(Relaxed),
        0,
        "a frame reached the decoder without its reference"
    );
    assert!(r.viewer.stats.lost.load(Relaxed) >= 1);
    assert_eq!(
        s.keys.load(Relaxed),
        1,
        "recovery must not need a key frame"
    );
    finish(r);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loss_of_final_frames_on_a_static_screen_still_recovers() {
    let r = rig(40, 120, 20_000).await;
    let s = r.s.clone();
    until("streaming", Duration::from_secs(5), || {
        s.decoded.lock().unwrap().len() >= 20
    })
    .await;
    // Lose everything from here until the capturer has gone idle.
    r.blackout.store(true, Relaxed);
    until("capture finished", Duration::from_secs(5), || {
        r.host.stats.encoded.load(Relaxed) + r.host.stats.skipped_no_room.load(Relaxed) >= 40
    })
    .await;
    let encoded = r.host.stats.encoded.load(Relaxed);
    tokio::time::sleep(Duration::from_millis(20)).await;
    r.blackout.store(false, Relaxed);
    until("idle refresh recovers", Duration::from_secs(5), || {
        s.invalidations.load(Relaxed) >= 1 && s.last_presented.load(Relaxed) >= encoded
    })
    .await;
    assert_eq!(s.ref_errors.load(Relaxed), 0);
    assert!(r.host.stats.idle_refreshes.load(Relaxed) >= 1);
    finish(r);
}
