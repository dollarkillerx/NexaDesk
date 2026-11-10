//! File transfer (§3.1 stream 4) over real QUIC, both ways, with hostile senders,
//! and sharing a constrained link with video under the §4.9 bulk allocation.
//!
//! Every received file goes to a temporary directory.

use std::{
    collections::VecDeque,
    net::{SocketAddr, UdpSocket},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed},
    },
    thread,
    time::{Duration, Instant},
};

use bytes::Bytes;
use nexa_media::*;
use nexa_net::{
    cc::transport_config,
    rate::RateConfig,
    vsock::{VIRTUAL_PEER, VirtualSocket, bind_media_socket},
};
use nexa_session::{
    bulk::{BulkLimiter, BulkRate, FixedRate},
    framed::{FramedRecv, FramedSend, accept_bi, open_bi},
    host::{HostMedia, HostParams},
    transfer::{
        self, FailReason, FileAnswer, FileBody, FileChunk, FileEnd, FileMsg, FileOffer, FileStatus,
        TransferConfig, TransferEvent, TransferLimits, TransferState, Transfers,
    },
    viewer::{ViewerMedia, ViewerParams},
};
use nexa_wire::{control::Mode, framing::StreamType};
use quinn::{
    ClientConfig, Connection, Endpoint, EndpointConfig, Runtime, ServerConfig, TokioRuntime,
    crypto::rustls::{QuicClientConfig, QuicServerConfig},
};
use rustls::{
    DigitallySignedStruct, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime},
};
use sha2::{Digest, Sha256};

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

/// A one-way link: `rate_bps` with a drop-tail queue of `queue` (0 = no limit).
#[derive(Clone, Copy)]
struct Shape {
    rate_bps: u64,
    queue: Duration,
}

/// Forwards viewer↔host; host → viewer goes through `shape` if given.
fn proxy(
    viewer: SocketAddr,
    host: SocketAddr,
    shape: Option<Shape>,
    stop: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
) -> (SocketAddr, SocketAddr) {
    let any: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let (a, b) = (
        bind_media_socket(any).unwrap(),
        bind_media_socket(any).unwrap(),
    );
    let addrs = (a.local_addr().unwrap(), b.local_addr().unwrap());
    for (rx, tx, dest, shaped) in [
        (a.try_clone().unwrap(), b.try_clone().unwrap(), host, None),
        (b, a, viewer, shape),
    ] {
        let (stop, dropped) = (stop.clone(), dropped.clone());
        thread::spawn(move || {
            rx.set_read_timeout(Some(Duration::from_micros(500)))
                .unwrap();
            let mut buf = [0u8; 2048];
            // (departure, packet)
            let mut queue: VecDeque<(Instant, Vec<u8>)> = VecDeque::new();
            let mut link_free = Instant::now();
            while !stop.load(Relaxed) {
                if let Ok((n, _)) = rx.recv_from(&mut buf) {
                    match shaped {
                        None => {
                            let _ = tx.send_to(&buf[..n], dest);
                        }
                        Some(s) => {
                            let now = Instant::now();
                            let start = link_free.max(now);
                            if s.queue > Duration::ZERO && start - now > s.queue {
                                dropped.fetch_add(1, Relaxed);
                            } else {
                                let cost = Duration::from_secs_f64(
                                    (n as f64 + 28.0) * 8.0 / s.rate_bps as f64,
                                );
                                link_free = start + cost;
                                queue.push_back((link_free, buf[..n].to_vec()));
                            }
                        }
                    }
                }
                let now = Instant::now();
                while queue.front().is_some_and(|(at, _)| *at <= now) {
                    let (_, p) = queue.pop_front().unwrap();
                    let _ = tx.send_to(&p, dest);
                }
            }
        });
    }
    addrs
}

struct Net {
    host: Connection,
    viewer: Connection,
    stop: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
    _eps: (Endpoint, Endpoint),
}

impl Drop for Net {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
    }
}

async fn connect(shape: Option<Shape>) -> Net {
    let stop = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicU64::new(0));
    let any: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let (vu, hu): (UdpSocket, UdpSocket) = (
        bind_media_socket(any).unwrap(),
        bind_media_socket(any).unwrap(),
    );
    let (va, ha) = (vu.local_addr().unwrap(), hu.local_addr().unwrap());
    let (to_host, to_viewer) = proxy(va, ha, shape, stop.clone(), dropped.clone());
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
        stop,
        dropped,
        _eps: (ve, he),
    }
}

#[derive(Default)]
struct Log(Mutex<Vec<TransferEvent>>);

impl Log {
    fn last(&self, id: u64) -> Option<TransferEvent> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|e| e.id == id)
            .cloned()
    }
    fn final_of(&self, dir: transfer::Direction) -> Option<TransferEvent> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|e| e.direction == dir && e.state.is_final())
            .cloned()
    }
    fn any(&self, f: impl Fn(&TransferEvent) -> bool) -> Option<TransferEvent> {
        self.0.lock().unwrap().iter().find(|e| f(e)).cloned()
    }
}

struct Side {
    transfers: Transfers,
    log: Arc<Log>,
    downloads: PathBuf,
}

fn side(conn: &Connection, dir: &Path, limits: TransferLimits, bulk: Arc<BulkLimiter>) -> Side {
    let log = Arc::new(Log::default());
    let l = log.clone();
    let downloads = dir.join("Downloads/NexaDesk");
    let transfers = Transfers::new(
        conn.clone(),
        TransferConfig {
            downloads: downloads.clone(),
            limits,
            bulk,
            confirm_timeout: Duration::from_secs(5),
        },
        Arc::new(move |e| l.0.lock().unwrap().push(e)),
    );
    Side {
        transfers,
        log,
        downloads,
    }
}

/// Accepts File streams on `conn` into `side`, or refuses them all.
fn serve(conn: &Connection, side: Option<Transfers>) -> tokio::task::JoinHandle<()> {
    let conn = conn.clone();
    tokio::spawn(async move {
        while let Ok((kind, send, recv)) = accept_bi(&conn).await {
            assert_eq!(kind, StreamType::File);
            match &side {
                Some(t) => t.accept(send, recv),
                None => transfer::refuse(send, recv),
            }
        }
    })
}

async fn until(what: &str, max: Duration, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + max;
    while !f() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Every file in `dir`, hidden ones included; empty if it does not exist.
fn files_in(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .map(|r| {
            r.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

fn payload(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

const LIMITS: TransferLimits = TransferLimits {
    max_size: 64 << 20,
    confirm_above: 32 << 20,
};

fn fast() -> Arc<BulkLimiter> {
    BulkLimiter::fixed(800_000_000)
}

async fn round_trip(sender_is_viewer: bool) {
    let net = connect(None).await;
    let dir = tempfile::tempdir().unwrap();
    let (tx_conn, rx_conn) = if sender_is_viewer {
        (&net.viewer, &net.host)
    } else {
        (&net.host, &net.viewer)
    };
    let sender = side(tx_conn, &dir.path().join("tx"), LIMITS, fast());
    let receiver = side(rx_conn, &dir.path().join("rx"), LIMITS, fast());
    let _serve = serve(rx_conn, Some(receiver.transfers.clone()));

    let src = dir.path().join("报告 final.bin");
    let data = payload(1_000_003, 9);
    std::fs::write(&src, &data).unwrap();
    let id = sender.transfers.send_file(src.clone());
    until("sent", Duration::from_secs(10), || {
        sender.log.last(id).is_some_and(|e| e.state.is_final())
    })
    .await;
    assert_eq!(
        sender.log.last(id).unwrap().state,
        TransferState::Done { path: None }
    );
    until("received", Duration::from_secs(5), || {
        receiver
            .log
            .final_of(transfer::Direction::Incoming)
            .is_some()
    })
    .await;
    let got = receiver
        .log
        .final_of(transfer::Direction::Incoming)
        .unwrap();
    let placed = receiver.downloads.join("报告 final.bin");
    assert_eq!(
        got.state,
        TransferState::Done {
            path: Some(placed.clone())
        }
    );
    assert_eq!((got.size, got.done), (1_000_003, 1_000_003));
    assert_eq!(std::fs::read(&placed).unwrap(), data);
    // Only the file: no temporary left behind.
    assert_eq!(files_in(&receiver.downloads), vec!["报告 final.bin"]);
    // The sender went through waiting and running with progress.
    assert!(
        sender
            .log
            .any(|e| e.id == id && e.state == TransferState::Running && e.done > 0)
            .is_some()
    );

    // The same name again lands beside it, never over it.
    let again = sender.transfers.send_file(src);
    until("second", Duration::from_secs(10), || {
        sender.log.last(again).is_some_and(|e| e.state.is_final())
    })
    .await;
    until("second placed", Duration::from_secs(5), || {
        files_in(&receiver.downloads).len() == 2
    })
    .await;
    assert_eq!(
        files_in(&receiver.downloads),
        vec!["报告 final (1).bin", "报告 final.bin"]
    );
    assert_eq!(std::fs::read(&placed).unwrap(), data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn viewer_to_host_round_trip_and_collision() {
    round_trip(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_to_viewer_round_trip_and_collision() {
    round_trip(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_peer_without_the_file_permission_writes_nothing() {
    let net = connect(None).await;
    let dir = tempfile::tempdir().unwrap();
    let sender = side(&net.viewer, dir.path(), LIMITS, fast());
    // The host side of a session without `file`: every File stream is refused unread.
    let _serve = serve(&net.host, None);
    let src = dir.path().join("a.txt");
    std::fs::write(&src, b"secret").unwrap();
    let id = sender.transfers.send_file(src);
    until("refused", Duration::from_secs(5), || {
        sender.log.last(id).is_some_and(|e| e.state.is_final())
    })
    .await;
    assert_eq!(
        sender.log.last(id).unwrap().state,
        TransferState::Failed(FailReason::NotPermitted)
    );
    assert!(!dir.path().join("rx").exists());
    assert!(files_in(&dir.path().join("Downloads/NexaDesk")).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversize_offers_are_refused_before_anything_is_written() {
    let net = connect(None).await;
    let dir = tempfile::tempdir().unwrap();
    let small = TransferLimits {
        max_size: 1000,
        confirm_above: 500,
    };
    let sender = side(&net.viewer, &dir.path().join("tx"), LIMITS, fast());
    let receiver = side(&net.host, &dir.path().join("rx"), small, fast());
    let _serve = serve(&net.host, Some(receiver.transfers.clone()));
    let src = dir.path().join("big.bin");
    std::fs::write(&src, payload(1001, 1)).unwrap();
    let id = sender.transfers.send_file(src);
    until("refused", Duration::from_secs(5), || {
        sender.log.last(id).is_some_and(|e| e.state.is_final())
    })
    .await;
    assert_eq!(
        sender.log.last(id).unwrap().state,
        TransferState::Failed(FailReason::TooLarge)
    );
    until("receiver says why", Duration::from_secs(2), || {
        receiver
            .log
            .final_of(transfer::Direction::Incoming)
            .is_some()
    })
    .await;
    assert_eq!(
        receiver
            .log
            .final_of(transfer::Direction::Incoming)
            .unwrap()
            .state,
        TransferState::Failed(FailReason::TooLarge)
    );
    assert!(
        !receiver.downloads.exists(),
        "not even the folder is created"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_files_wait_for_the_receiver_to_accept() {
    let net = connect(None).await;
    let dir = tempfile::tempdir().unwrap();
    let limits = TransferLimits {
        max_size: 1 << 20,
        confirm_above: 1000,
    };
    let sender = side(&net.viewer, &dir.path().join("tx"), LIMITS, fast());
    let receiver = side(&net.host, &dir.path().join("rx"), limits, fast());
    let _serve = serve(&net.host, Some(receiver.transfers.clone()));
    let src = dir.path().join("big.bin");
    std::fs::write(&src, payload(5000, 2)).unwrap();

    // Declined: nothing written.
    let id = sender.transfers.send_file(src.clone());
    until("asked", Duration::from_secs(5), || {
        receiver
            .log
            .any(|e| e.state == TransferState::NeedsConfirm)
            .is_some()
    })
    .await;
    assert!(!receiver.downloads.exists());
    let asked = receiver
        .log
        .any(|e| e.state == TransferState::NeedsConfirm)
        .unwrap();
    assert_eq!((asked.name.as_str(), asked.size), ("big.bin", 5000));
    assert!(receiver.transfers.answer(asked.id, false));
    assert!(!receiver.transfers.answer(asked.id, true), "answered once");
    until("declined", Duration::from_secs(5), || {
        sender.log.last(id).is_some_and(|e| e.state.is_final())
    })
    .await;
    assert_eq!(
        sender.log.last(id).unwrap().state,
        TransferState::Failed(FailReason::Declined)
    );
    assert!(files_in(&receiver.downloads).is_empty());

    // Accepted: arrives.
    let id = sender.transfers.send_file(src);
    until("asked again", Duration::from_secs(5), || {
        receiver
            .log
            .any(|e| e.state == TransferState::NeedsConfirm && e.id != asked.id)
            .is_some()
    })
    .await;
    let second = receiver
        .log
        .any(|e| e.state == TransferState::NeedsConfirm && e.id != asked.id)
        .unwrap();
    assert!(receiver.transfers.answer(second.id, true));
    until("done", Duration::from_secs(5), || {
        sender.log.last(id).is_some_and(|e| e.state.is_final())
    })
    .await;
    assert_eq!(
        sender.log.last(id).unwrap().state,
        TransferState::Done { path: None }
    );
    until("placed", Duration::from_secs(2), || {
        files_in(&receiver.downloads) == vec!["big.bin"]
    })
    .await;

    // Unanswered: declined when the person's time runs out (5 s here).
    let id = sender.transfers.send_file(dir.path().join("big.bin"));
    until("timed out", Duration::from_secs(12), || {
        sender.log.last(id).is_some_and(|e| e.state.is_final())
    })
    .await;
    assert_eq!(
        sender.log.last(id).unwrap().state,
        TransferState::Failed(FailReason::Declined)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn either_side_can_cancel_and_nothing_is_left() {
    let net = connect(None).await;
    let dir = tempfile::tempdir().unwrap();
    // 4 Mbit/s: a 2 MB file takes four seconds, time enough to cancel.
    let slow = || BulkLimiter::new(Arc::new(FixedRate(4_000_000)) as Arc<dyn BulkRate>);
    let sender = side(&net.viewer, &dir.path().join("tx"), LIMITS, slow());
    let receiver = side(&net.host, &dir.path().join("rx"), LIMITS, slow());
    let _serve = serve(&net.host, Some(receiver.transfers.clone()));
    let src = dir.path().join("slow.bin");
    std::fs::write(&src, payload(2_000_000, 3)).unwrap();

    // The sender cancels.
    let id = sender.transfers.send_file(src.clone());
    until("under way", Duration::from_secs(5), || {
        receiver
            .log
            .any(|e| e.state == TransferState::Running && e.done > 0)
            .is_some()
    })
    .await;
    assert_eq!(files_in(&receiver.downloads).len(), 1, "the temporary file");
    assert!(files_in(&receiver.downloads)[0].ends_with(".nexadesk-part"));
    assert!(files_in(&receiver.downloads)[0].starts_with('.'));
    assert!(sender.transfers.cancel(id));
    until("cancelled", Duration::from_secs(5), || {
        receiver
            .log
            .final_of(transfer::Direction::Incoming)
            .is_some()
    })
    .await;
    assert_eq!(
        sender.log.last(id).unwrap().state,
        TransferState::Cancelled { by_peer: false }
    );
    assert_eq!(
        receiver
            .log
            .final_of(transfer::Direction::Incoming)
            .unwrap()
            .state,
        TransferState::Cancelled { by_peer: true }
    );
    until("temp removed", Duration::from_secs(2), || {
        files_in(&receiver.downloads).is_empty()
    })
    .await;

    // The receiver cancels.
    let count = receiver.log.0.lock().unwrap().len();
    let id = sender.transfers.send_file(src);
    until("under way again", Duration::from_secs(5), || {
        receiver.log.0.lock().unwrap()[count..]
            .iter()
            .any(|e| e.state == TransferState::Running && e.done > 0)
    })
    .await;
    let incoming = receiver.log.0.lock().unwrap()[count..].last().unwrap().id;
    assert!(receiver.transfers.cancel(incoming));
    until("sender told", Duration::from_secs(5), || {
        sender.log.last(id).is_some_and(|e| e.state.is_final())
    })
    .await;
    assert_eq!(
        sender.log.last(id).unwrap().state,
        TransferState::Cancelled { by_peer: true }
    );
    assert_eq!(
        receiver.log.last(incoming).unwrap().state,
        TransferState::Cancelled { by_peer: false }
    );
    until("temp removed again", Duration::from_secs(2), || {
        files_in(&receiver.downloads).is_empty()
    })
    .await;
}

// ---------------------------------------------------------------------------------
// Hostile senders, speaking the protocol by hand
// ---------------------------------------------------------------------------------

async fn send(tx: &mut FramedSend, body: FileBody) {
    tx.send(&FileMsg::from(body)).await.unwrap();
}

async fn recv(rx: &mut FramedRecv) -> Option<FileBody> {
    tokio::time::timeout(Duration::from_secs(5), rx.next::<FileMsg>())
        .await
        .expect("an answer")
        .ok()
        .flatten()
        .and_then(|m| m.body)
}

fn status(b: Option<FileBody>) -> FileStatus {
    match b {
        Some(FileBody::Answer(FileAnswer { status })) => FileStatus::try_from(status).unwrap(),
        Some(FileBody::Result(r)) => FileStatus::try_from(r.status).unwrap(),
        other => panic!("expected an answer or a result, got {other:?}"),
    }
}

struct Hostile {
    net: Net,
    receiver: Side,
    _dir: tempfile::TempDir,
    _serve: tokio::task::JoinHandle<()>,
}

async fn hostile() -> Hostile {
    let net = connect(None).await;
    let dir = tempfile::tempdir().unwrap();
    let receiver = side(&net.host, dir.path(), LIMITS, fast());
    let serve = serve(&net.host, Some(receiver.transfers.clone()));
    Hostile {
        net,
        receiver,
        _dir: dir,
        _serve: serve,
    }
}

impl Hostile {
    async fn open(&self, name: &str, size: u64) -> (FramedSend, FramedRecv, FileStatus) {
        let (mut tx, mut rx) = open_bi(&self.net.viewer, StreamType::File).await.unwrap();
        send(
            &mut tx,
            FileBody::Offer(FileOffer {
                name: name.into(),
                size,
            }),
        )
        .await;
        let s = status(recv(&mut rx).await);
        (tx, rx, s)
    }

    async fn settle(&self) {
        until("temporary files removed", Duration::from_secs(3), || {
            files_in(&self.receiver.downloads)
                .iter()
                .all(|f| !f.ends_with(".nexadesk-part"))
        })
        .await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_hash_discards_the_file() {
    let h = hostile().await;
    let data = payload(100_000, 4);
    let (mut tx, mut rx, s) = h.open("tampered.bin", data.len() as u64).await;
    assert_eq!(s, FileStatus::Ok);
    for (i, chunk) in data.chunks(transfer::CHUNK).enumerate() {
        send(
            &mut tx,
            FileBody::Chunk(FileChunk {
                offset: (i * transfer::CHUNK) as u64,
                data: Bytes::copy_from_slice(chunk),
            }),
        )
        .await;
    }
    let mut wrong = Sha256::digest(&data).to_vec();
    wrong[0] ^= 1;
    send(&mut tx, FileBody::End(FileEnd { sha256: wrong })).await;
    assert_eq!(status(recv(&mut rx).await), FileStatus::HashMismatch);
    h.settle().await;
    assert!(files_in(&h.receiver.downloads).is_empty());
    assert_eq!(
        h.receiver
            .log
            .final_of(transfer::Direction::Incoming)
            .unwrap()
            .state,
        TransferState::Failed(FailReason::HashMismatch)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inconsistent_chunks_and_ends_are_rejected() {
    let h = hostile().await;
    let chunk = |offset, len| {
        FileBody::Chunk(FileChunk {
            offset,
            data: Bytes::from(vec![1u8; len]),
        })
    };
    let good_hash = |len| Sha256::digest(vec![1u8; len]).to_vec();
    // A gap, an overlap, an empty chunk, too much data, an early end, a short hash, a
    // second offer.
    let cases: Vec<(u64, Vec<FileBody>)> = vec![
        (100, vec![chunk(10, 10)]),
        (100, vec![chunk(0, 50), chunk(40, 10)]),
        (100, vec![chunk(0, 0)]),
        (100, vec![chunk(0, 101)]),
        (
            100,
            vec![
                chunk(0, 50),
                FileBody::End(FileEnd {
                    sha256: good_hash(50),
                }),
            ],
        ),
        (
            50,
            vec![
                chunk(0, 50),
                FileBody::End(FileEnd {
                    sha256: vec![0; 16],
                }),
            ],
        ),
        (
            100,
            vec![FileBody::Offer(FileOffer {
                name: "b".into(),
                size: 1,
            })],
        ),
    ];
    for (i, (size, bodies)) in cases.into_iter().enumerate() {
        let (mut tx, mut rx, s) = h.open(&format!("case{i}.bin"), size).await;
        assert_eq!(s, FileStatus::Ok, "case {i}");
        for b in bodies {
            send(&mut tx, b).await;
        }
        assert_eq!(status(recv(&mut rx).await), FileStatus::Invalid, "case {i}");
    }
    h.settle().await;
    assert!(files_in(&h.receiver.downloads).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offered_names_are_cleaned_or_refused() {
    let h = hostile().await;
    for bad in ["../../etc/passwd", "a/b", "..", "\u{202E}", "x\0y"] {
        let (_tx, _rx, s) = h.open(bad, 3).await;
        assert_eq!(s, FileStatus::Invalid, "{bad:?}");
    }
    let (mut tx, mut rx, s) = h.open(".zshrc", 3).await;
    assert_eq!(s, FileStatus::Ok);
    send(
        &mut tx,
        FileBody::Chunk(FileChunk {
            offset: 0,
            data: Bytes::from_static(b"abc"),
        }),
    )
    .await;
    send(
        &mut tx,
        FileBody::End(FileEnd {
            sha256: Sha256::digest(b"abc").to_vec(),
        }),
    )
    .await;
    assert_eq!(status(recv(&mut rx).await), FileStatus::Ok);
    assert_eq!(files_in(&h.receiver.downloads), vec!["zshrc"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn garbage_on_a_file_stream_is_survived() {
    let h = hostile().await;
    for junk in [vec![0xFFu8; 64], vec![0x05, 0x0A, 0x03], vec![0x80; 20]] {
        let (mut tx, _rx) = open_bi(&h.net.viewer, StreamType::File).await.unwrap();
        let _ = tx.send_body(&junk).await;
        tx.finish();
    }
    // A raw oversized frame header.
    let (mut tx, _rx) = open_bi(&h.net.viewer, StreamType::File).await.unwrap();
    let _ = tx.send_body(&vec![0u8; 300_000]).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    // Still serving.
    let (mut tx, mut rx, s) = h.open("after.txt", 1).await;
    assert_eq!(s, FileStatus::Ok);
    send(
        &mut tx,
        FileBody::Chunk(FileChunk {
            offset: 0,
            data: Bytes::from_static(b"z"),
        }),
    )
    .await;
    send(
        &mut tx,
        FileBody::End(FileEnd {
            sha256: Sha256::digest(b"z").to_vec(),
        }),
    )
    .await;
    assert_eq!(status(recv(&mut rx).await), FileStatus::Ok);
    assert_eq!(files_in(&h.receiver.downloads), vec!["after.txt"]);
}

// ---------------------------------------------------------------------------------
// Sharing a constrained link with video (§4.9)
// ---------------------------------------------------------------------------------

fn image() -> NativeImage {
    NativeImage::Cpu(Arc::new(CpuFrame {
        width: 1,
        height: 1,
        y: vec![0],
        uv: vec![0],
    }))
}

struct EndlessCapturer {
    interval: Duration,
    next_at: Instant,
}

impl Capturer for EndlessCapturer {
    fn next(&mut self, _: Duration) -> Result<CaptureEvent> {
        thread::sleep(self.next_at.saturating_duration_since(Instant::now()));
        self.next_at += self.interval;
        Ok(CaptureEvent::Frame(CapturedFrame {
            image: image(),
            captured_at: Instant::now(),
            dirty: vec![],
        }))
    }
}

/// Frames sized to the bitrate the rate controller programs; every one a key frame
/// so loss never stalls decoding.
struct SizedEncoder {
    bps: Arc<AtomicU32>,
    fps: u32,
    /// What the content needs at most (a mostly static screen needs little).
    content_bps: u32,
}

impl Encoder for SizedEncoder {
    fn caps(&self) -> EncoderCaps {
        EncoderCaps::default()
    }
    fn encode(
        &mut self,
        _: &CapturedFrame,
        id: FrameId,
        _: EncodeOpts,
        out: &mut Vec<u8>,
    ) -> Result<EncodedMeta> {
        let size = (self.bps.load(Relaxed).min(self.content_bps) / 8 / self.fps).max(64) as usize;
        out.resize(size, 7);
        Ok(EncodedMeta {
            frame_id: id,
            key: true,
            recovery_point: true,
        })
    }
    fn set_bitrate(&mut self, bps: u32) -> Result<()> {
        self.bps.store(bps, Relaxed);
        Ok(())
    }
    fn invalidate_from(&mut self, _: FrameId) -> Result<Recovery> {
        Ok(Recovery::Idr)
    }
    fn on_frame_acked(&mut self, _: FrameId) {}
    fn force_idr(&mut self) {}
}

struct CountingDecoder;

impl Decoder for CountingDecoder {
    fn decode(&mut self, _: &[u8], meta: FrameMeta) -> Result<DecodedFrame> {
        Ok(DecodedFrame {
            image: image(),
            meta,
        })
    }
}

struct NullPresenter;

impl Presenter for NullPresenter {
    fn resize(&mut self, _: u32, _: u32, _: f32) -> Result<()> {
        Ok(())
    }
    fn set_mode(&mut self, _: PresentMode) -> Result<()> {
        Ok(())
    }
    fn present(&mut self, _: &DecodedFrame) -> Result<PresentStat> {
        Ok(PresentStat::default())
    }
}

struct Outcome {
    /// Of frames completed or lost during the transfer, the share lost.
    video_loss: f64,
    /// Video bitrate the controller settled on, bits/s.
    video_bps: u64,
    /// File bytes per second.
    file_rate: f64,
}

/// A 20 Mbit/s host → viewer link with 50 ms of queue; video at 60 fps needing up
/// to `content_bps`, while the host sends a file paced by `bulk_of(media)`.
async fn share_link(
    content_bps: u32,
    bulk_of: impl FnOnce(&HostMedia) -> Arc<dyn BulkRate>,
) -> Outcome {
    const FPS: u32 = 60;
    let net = connect(Some(Shape {
        rate_bps: 20_000_000,
        queue: Duration::from_millis(50),
    }))
    .await;
    let bps = Arc::new(AtomicU32::new(6_000_000));
    let media = HostMedia::spawn(
        net.host.clone(),
        Box::new(EndlessCapturer {
            interval: Duration::from_secs_f64(1.0 / f64::from(FPS)),
            next_at: Instant::now(),
        }),
        Box::new(SizedEncoder {
            bps: bps.clone(),
            fps: FPS,
            content_bps,
        }),
        HostParams {
            stream: 0,
            fps: FPS,
            mode: Mode::Balanced,
            rate: RateConfig {
                initial_bps: 6_000_000,
                min_bps: 300_000,
                max_bps: 200_000_000,
            },
        },
        Box::new(|_| {}),
    );
    let viewer = ViewerMedia::spawn(
        net.viewer.clone(),
        Box::new(CountingDecoder),
        Box::new(NullPresenter),
        ViewerParams {
            stream: 0,
            frame_deadline: Duration::from_millis(60),
            reorder_grace: Duration::from_millis(2),
        },
    );
    // Video alone first, so the controller finds its feet.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let dir = tempfile::tempdir().unwrap();
    let bulk = BulkLimiter::new(bulk_of(&media));
    let roomy = TransferLimits {
        max_size: 1 << 30,
        confirm_above: 1 << 30,
    };
    let sender = side(&net.host, &dir.path().join("tx"), roomy, bulk);
    let receiver = side(&net.viewer, &dir.path().join("rx"), roomy, fast());
    let _serve = serve(&net.viewer, Some(receiver.transfers.clone()));
    let src = dir.path().join("big.bin");
    std::fs::write(&src, payload(48 << 20, 5)).unwrap();

    let (c0, l0) = (
        viewer.stats.complete.load(Relaxed),
        viewer.stats.lost.load(Relaxed),
    );
    let id = sender.transfers.send_file(src);
    // The bulk rate ramps with the controller's target; measure the second half.
    let rate = media.bulk_rate();
    for _ in 0..8 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        eprintln!(
            "bulk {:.2} Mbit/s",
            rate.bulk_bps(Instant::now()) as f64 / 1e6
        );
    }
    let halfway = sender.log.last(id).map_or(0, |e| e.done);
    let started = Instant::now();
    tokio::time::sleep(Duration::from_secs(4)).await;
    let elapsed = started.elapsed().as_secs_f64();
    let (c1, l1) = (
        viewer.stats.complete.load(Relaxed),
        viewer.stats.lost.load(Relaxed),
    );
    let sent = sender.log.last(id).map_or(0, |e| e.done) - halfway;
    sender.transfers.cancel(id);
    let (complete, lost) = (c1 - c0, l1 - l0);
    let outcome = Outcome {
        video_loss: lost as f64 / (complete + lost).max(1) as f64,
        video_bps: u64::from(bps.load(Relaxed).min(content_bps)),
        file_rate: sent as f64 / elapsed,
    };
    eprintln!(
        "video loss {:.1}% ({complete} complete, {lost} lost), encoder {:.1} Mbit/s, file {:.2} Mbit/s, link drops {}",
        outcome.video_loss * 100.0,
        outcome.video_bps as f64 / 1e6,
        outcome.file_rate * 8.0 / 1e6,
        net.dropped.load(Relaxed)
    );
    viewer.stop();
    media.stop();
    outcome
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_transfer_takes_only_what_video_leaves_on_a_constrained_link() {
    // Video that would take everything: the file gets the floor, video is untouched.
    let busy = share_link(u32::MAX, |m| Arc::new(m.bulk_rate())).await;
    assert!(
        busy.file_rate * 8.0 >= 500_000.0,
        "the file still moves: {:.2} Mbit/s",
        busy.file_rate * 8.0 / 1e6
    );
    assert!(
        busy.video_loss < 0.05,
        "video keeps flowing: {:.1}% lost",
        busy.video_loss * 100.0
    );

    // Video that needs 4 Mbit/s: the file takes much of the rest, video still fine.
    let shared = share_link(4_000_000, |m| Arc::new(m.bulk_rate())).await;
    assert!(
        shared.file_rate * 8.0 >= 3_000_000.0 && shared.file_rate > busy.file_rate * 2.0,
        "the file uses what video leaves: {:.2} Mbit/s",
        shared.file_rate * 8.0 / 1e6
    );
    assert!(
        shared.video_loss < 0.05,
        "video keeps flowing: {:.1}% lost",
        shared.video_loss * 100.0
    );
    assert!(
        shared.file_rate * 8.0 + shared.video_bps as f64 <= 21_000_000.0,
        "together within the link"
    );

    // The same transfer without the allocation floods the link.
    let unpaced = share_link(4_000_000, |_| Arc::new(FixedRate(400_000_000))).await;
    assert!(
        unpaced.video_loss > shared.video_loss + 0.05,
        "unpaced bulk hurts video: {:.1}% vs {:.1}%",
        unpaced.video_loss * 100.0,
        shared.video_loss * 100.0
    );
}
