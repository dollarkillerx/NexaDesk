//! The Tailscale module (§4.7): tailnet addresses from the interfaces, and,
//! when tailscaled's LocalAPI is reachable, the device list and whether each
//! peer is reached directly or through DERP.
//!
//! The LocalAPI is an internal interface: every failure here is silent and
//! leaves address-only discovery (§5.4 level 1). Nothing depends on it.

use std::{
    io::{Read, Write},
    net::{IpAddr, SocketAddr, UdpSocket},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, Ordering::Relaxed},
    },
    time::{Duration, Instant},
};

use crate::{check::is_tailnet, gather::system_addresses};

/// macOS (open-source tailscaled) and Linux. The App Store build's TCP + token
/// LocalAPI and Windows' named pipe are not supported: address-only there.
pub const SOCKET_PATHS: [&str; 2] = [
    "/var/run/tailscaled.socket",
    "/var/run/tailscale/tailscaled.sock",
];
pub const LOCALAPI_TIMEOUT: Duration = Duration::from_millis(500);
const MAX_STATUS: usize = 8 << 20;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TailscaleInfo {
    pub self_ips: Vec<IpAddr>,
    pub peers: Vec<TsPeer>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TsPeer {
    pub name: String,
    /// MagicDNS name without the trailing dot, if any.
    pub dns_name: String,
    pub os: String,
    pub ips: Vec<IpAddr>,
    pub online: bool,
    /// `Some(true)`: a direct WireGuard path (`CurAddr` set). `Some(false)`:
    /// through DERP. `None`: offline or unknown.
    pub direct: Option<bool>,
    /// The DERP region code, when relayed.
    pub relay: String,
}

impl TailscaleInfo {
    pub fn peer_by_ip(&self, ip: IpAddr) -> Option<&TsPeer> {
        let ip = ip.to_canonical();
        self.peers.iter().find(|p| p.ips.contains(&ip))
    }
}

/// Tailnet addresses on this machine's interfaces. Never fails.
pub fn discover_local() -> Vec<IpAddr> {
    let mut out: Vec<IpAddr> = system_addresses()
        .into_iter()
        .map(|(_, ip)| ip)
        .filter(|ip| is_tailnet(*ip))
        .collect();
    out.dedup();
    out
}

/// `/localapi/v0/status` from the first socket that answers; `None` on any failure.
pub fn query_localapi() -> Option<TailscaleInfo> {
    SOCKET_PATHS
        .iter()
        .find_map(|p| query_localapi_at(Path::new(p), LOCALAPI_TIMEOUT))
}

#[cfg(unix)]
pub fn query_localapi_at(path: &Path, timeout: Duration) -> Option<TailscaleInfo> {
    use std::os::unix::net::UnixStream;
    let deadline = Instant::now() + timeout;
    let mut s = UnixStream::connect(path).ok()?;
    // Both timeouts before the first byte: macOS refuses setsockopt (EINVAL)
    // once the peer has closed, which a fast server may already have done.
    s.set_write_timeout(Some(timeout)).ok()?;
    s.set_read_timeout(Some(timeout)).ok()?;
    s.write_all(
        b"GET /localapi/v0/status HTTP/1.1\r\nHost: local-tailscaled.sock\r\nSec-Tailscale: localapi\r\nConnection: close\r\n\r\n",
    )
    .ok()?;
    let mut raw = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return None;
        }
        let _ = s.set_read_timeout(Some(left));
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if raw.len() > MAX_STATUS {
                    return None;
                }
            }
            Err(_) => return None,
        }
    }
    let body = http_body(&raw)?;
    parse_status(&body)
}

#[cfg(not(unix))]
pub fn query_localapi_at(_: &Path, _: Duration) -> Option<TailscaleInfo> {
    None
}

/// The body of a complete `200` response, de-chunked.
fn http_body(raw: &[u8]) -> Option<Vec<u8>> {
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&raw[..split]).ok()?;
    let body = &raw[split + 4..];
    if head.split_whitespace().nth(1) != Some("200") {
        return None;
    }
    let chunked = head.lines().any(|l| {
        l.split_once(':').is_some_and(|(k, v)| {
            k.trim().eq_ignore_ascii_case("transfer-encoding")
                && v.trim().eq_ignore_ascii_case("chunked")
        })
    });
    if !chunked {
        return Some(body.to_vec());
    }
    let mut out = Vec::new();
    let mut rest = body;
    loop {
        let eol = rest.windows(2).position(|w| w == b"\r\n")?;
        let size_text = std::str::from_utf8(&rest[..eol]).ok()?;
        let size = usize::from_str_radix(size_text.split(';').next()?.trim(), 16).ok()?;
        rest = &rest[eol + 2..];
        if size == 0 {
            return Some(out);
        }
        if rest.len() < size + 2 {
            return None;
        }
        out.extend_from_slice(&rest[..size]);
        rest = &rest[size + 2..];
    }
}

fn ips_of(v: &serde_json::Value) -> Vec<IpAddr> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str()?.parse::<IpAddr>().ok())
                .filter(|ip| is_tailnet(*ip))
                .collect()
        })
        .unwrap_or_default()
}

fn text(v: &serde_json::Value, key: &str, max: usize) -> String {
    v.get(key)
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .take(max)
        .collect()
}

/// Parses a status document. Only tailnet-range addresses are kept: whatever
/// the document says, these are the only ones treated as Tailscale.
pub fn parse_status(json: &[u8]) -> Option<TailscaleInfo> {
    let v: serde_json::Value = serde_json::from_slice(json).ok()?;
    if v.get("BackendState").and_then(|s| s.as_str()) != Some("Running") {
        return None;
    }
    let self_ips = v
        .get("Self")
        .map(|s| ips_of(&s["TailscaleIPs"]))
        .unwrap_or_default();
    let mut peers: Vec<TsPeer> = v
        .get("Peer")
        .and_then(|p| p.as_object())
        .map(|m| {
            m.values()
                .take(4096)
                .map(|p| {
                    let online = p.get("Online").and_then(|o| o.as_bool()).unwrap_or(false);
                    let cur = text(p, "CurAddr", 64);
                    let relay = text(p, "Relay", 16);
                    TsPeer {
                        name: text(p, "HostName", 64),
                        dns_name: text(p, "DNSName", 253).trim_end_matches('.').to_owned(),
                        os: text(p, "OS", 32),
                        ips: ips_of(&p["TailscaleIPs"]),
                        online,
                        direct: online.then_some(!cur.is_empty()),
                        relay,
                    }
                })
                .filter(|p| !p.ips.is_empty())
                .collect()
        })
        .unwrap_or_default();
    peers.sort_by(|a, b| a.name.cmp(&b.name).then(a.dns_name.cmp(&b.dns_name)));
    Some(TailscaleInfo { self_ips, peers })
}

/// Where a [`TailnetMonitor`] reads from.
#[derive(Clone, Debug)]
pub enum LocalApi {
    /// The standard socket paths.
    System,
    /// One socket, e.g. a fake in tests.
    Socket(PathBuf),
}

/// The latest LocalAPI status, refreshed in the background.
#[derive(Debug, Default)]
pub struct TailnetMonitor {
    latest: Mutex<Option<TailscaleInfo>>,
    generation: AtomicU64,
}

impl TailnetMonitor {
    /// Polls every `every` until the last `Arc` is dropped.
    pub fn spawn(source: LocalApi, every: Duration) -> Arc<Self> {
        let me = Arc::new(Self::default());
        let weak: Weak<Self> = Arc::downgrade(&me);
        let _ = std::thread::Builder::new()
            .name("tailnet".into())
            .spawn(move || {
                loop {
                    let info = match &source {
                        LocalApi::System => query_localapi(),
                        LocalApi::Socket(p) => query_localapi_at(p, LOCALAPI_TIMEOUT),
                    };
                    let Some(me) = weak.upgrade() else { return };
                    me.set(info);
                    drop(me);
                    std::thread::sleep(every);
                }
            });
        me
    }

    /// A monitor that is only ever fed by hand (tests).
    pub fn fixed(info: Option<TailscaleInfo>) -> Arc<Self> {
        let me = Arc::new(Self::default());
        me.set(info);
        me
    }

    pub fn set(&self, info: Option<TailscaleInfo>) {
        let mut l = self.latest.lock().unwrap_or_else(|e| e.into_inner());
        if *l != info {
            *l = info;
            self.generation.fetch_add(1, Relaxed);
        }
    }

    pub fn snapshot(&self) -> Option<TailscaleInfo> {
        self.latest
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Bumps whenever the status changes.
    pub fn generation(&self) -> u64 {
        self.generation.load(Relaxed)
    }

    /// Whether the peer at `ip` is reached directly (`Some(true)`) or over DERP.
    pub fn direct(&self, ip: IpAddr) -> Option<bool> {
        self.latest
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()?
            .peer_by_ip(ip)?
            .direct
    }
}

/// Does a QUIC server listen at `addr`? Sends one Initial-sized packet with a
/// reserved version (RFC 9000 §15) and waits for Version Negotiation. No
/// connection is made, so the host sees nothing. Any QUIC server on that port
/// counts: the port is NexaDesk's, but this does not prove it.
pub fn probe_quic(addr: SocketAddr, timeout: Duration, rng: &mut dyn FnMut(&mut [u8])) -> bool {
    let bind: SocketAddr = if addr.is_ipv4() {
        "0.0.0.0:0".parse().unwrap()
    } else {
        "[::]:0".parse().unwrap()
    };
    let Ok(s) = UdpSocket::bind(bind) else {
        return false;
    };
    if s.connect(addr).is_err() {
        return false;
    }
    let mut cids = [0u8; 16];
    rng(&mut cids);
    let (dcid, scid) = cids.split_at(8);
    let mut p = Vec::with_capacity(1200);
    p.push(0xC0);
    p.extend_from_slice(&[0x1A, 0x2A, 0x3A, 0x4A]);
    p.push(8);
    p.extend_from_slice(dcid);
    p.push(8);
    p.extend_from_slice(scid);
    p.resize(1200, 0);
    if s.send(&p).is_err() {
        return false;
    }
    let deadline = Instant::now() + timeout;
    let mut buf = [0u8; 1500];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || s.set_read_timeout(Some(left)).is_err() {
            return false;
        }
        let Ok(n) = s.recv(&mut buf) else {
            return false;
        };
        let b = &buf[..n];
        // Version Negotiation: long header, version 0, our SCID echoed as its DCID.
        if n >= 7 + 16
            && b[0] & 0x80 != 0
            && b[1..5] == [0, 0, 0, 0]
            && b[5] == 8
            && &b[6..14] == scid
        {
            return true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS: &str = r#"{
      "BackendState": "Running",
      "Self": {"HostName": "me", "TailscaleIPs": ["100.64.0.1", "fd7a:115c:a1e0::1"]},
      "Peer": {
        "nodekey:a": {"HostName": "studio", "DNSName": "studio.tail1234.ts.net.", "OS": "macOS",
                      "TailscaleIPs": ["100.64.0.2", "fd7a:115c:a1e0::2"], "Online": true,
                      "CurAddr": "192.168.1.5:41641", "Relay": "fra"},
        "nodekey:b": {"HostName": "laptop", "DNSName": "laptop.tail1234.ts.net.", "OS": "windows",
                      "TailscaleIPs": ["100.64.0.3"], "Online": true, "CurAddr": "", "Relay": "nyc"},
        "nodekey:c": {"HostName": "old\u0007pc", "TailscaleIPs": ["100.64.0.4", "8.8.8.8"], "Online": false},
        "nodekey:d": {"HostName": "liar", "TailscaleIPs": ["192.168.1.9"], "Online": true}
      }
    }"#;

    #[test]
    fn parses_peers_and_direct_or_derp() {
        let info = parse_status(STATUS.as_bytes()).unwrap();
        assert_eq!(info.self_ips.len(), 2);
        let names: Vec<&str> = info.peers.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["laptop", "oldpc", "studio"],
            "no peer without a tailnet IP"
        );
        let studio = info.peer_by_ip("100.64.0.2".parse().unwrap()).unwrap();
        assert_eq!(studio.direct, Some(true));
        assert_eq!(studio.dns_name, "studio.tail1234.ts.net");
        let laptop = info.peer_by_ip("100.64.0.3".parse().unwrap()).unwrap();
        assert_eq!((laptop.direct, laptop.relay.as_str()), (Some(false), "nyc"));
        let old = info.peer_by_ip("100.64.0.4".parse().unwrap()).unwrap();
        assert_eq!(old.direct, None);
        assert_eq!(old.ips, vec!["100.64.0.4".parse::<IpAddr>().unwrap()]);
        assert!(parse_status(br#"{"BackendState":"Stopped"}"#).is_none());
        assert!(parse_status(b"not json").is_none());
    }

    fn fake_localapi(dir: &Path, response: Vec<u8>) -> PathBuf {
        use std::os::unix::net::UnixListener;
        let path = dir.join("tailscaled.sock");
        let l = UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            for c in l.incoming() {
                let Ok(mut c) = c else { return };
                let mut req = [0u8; 1024];
                let _ = c.read(&mut req);
                let _ = c.write_all(&response);
            }
        });
        path
    }

    #[test]
    fn reads_a_fake_localapi_including_chunked() {
        let dir = tempdir();
        let mut chunked = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        for part in STATUS.as_bytes().chunks(100) {
            chunked.extend_from_slice(format!("{:x}\r\n", part.len()).as_bytes());
            chunked.extend_from_slice(part);
            chunked.extend_from_slice(b"\r\n");
        }
        chunked.extend_from_slice(b"0\r\n\r\n");
        let path = fake_localapi(&dir, chunked);
        let info = query_localapi_at(&path, Duration::from_secs(2)).unwrap();
        assert_eq!(info.peers.len(), 3);

        let mon = TailnetMonitor::spawn(LocalApi::Socket(path), Duration::from_millis(50));
        let t = Instant::now();
        while mon.snapshot().is_none() && t.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(mon.direct("100.64.0.3".parse().unwrap()), Some(false));
        assert_eq!(mon.direct("100.64.0.2".parse().unwrap()), Some(true));
        assert_eq!(mon.direct("100.64.0.99".parse().unwrap()), None);
    }

    #[test]
    fn failures_are_silent() {
        let dir = tempdir();
        assert!(query_localapi_at(&dir.join("nothing.sock"), Duration::from_millis(100)).is_none());
        let refused = fake_localapi(&dir, b"HTTP/1.1 403 Forbidden\r\n\r\n".to_vec());
        assert!(query_localapi_at(&refused, Duration::from_millis(500)).is_none());
        let _ = discover_local();
    }

    fn tempdir() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_micros();
        let p = std::env::temp_dir().join(format!(
            "nexa-ts-{}-{t}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[tokio::test]
    async fn quic_probe_sees_a_quic_server_and_nothing_else() {
        let cert = rcgen::generate_simple_self_signed(vec!["x".into()]).unwrap();
        let key =
            rustls::pki_types::PrivateKeyDer::try_from(cert.signing_key.serialize_der()).unwrap();
        let cfg = quinn::ServerConfig::with_single_cert(
            vec![rustls::pki_types::CertificateDer::from(
                cert.cert.der().to_vec(),
            )],
            key,
        )
        .unwrap();
        let ep = quinn::Endpoint::server(cfg, "127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = ep.local_addr().unwrap();
        let mut r = |b: &mut [u8]| b.iter_mut().enumerate().for_each(|(i, x)| *x = i as u8 + 1);
        let found =
            tokio::task::spawn_blocking(move || probe_quic(addr, Duration::from_secs(1), &mut r))
                .await
                .unwrap();
        assert!(found);
        let plain = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let other = plain.local_addr().unwrap();
        let mut r = |b: &mut [u8]| b.fill(3);
        assert!(!probe_quic(other, Duration::from_millis(200), &mut r));
    }
}
