//! Port mapping (§4.2 Portmap): ask the gateway to forward a UDP port to the
//! session socket, so the peer can reach it directly even when our NAT filters.
//!
//! Tried in order PCP (RFC 6887), NAT-PMP (RFC 6886), UPnP IGD, all within
//! [`PORTMAP_TIMEOUT`]. SSDP discovery runs from the start, in parallel, so
//! UPnP still fits when the first two time out. Blocking I/O: call it from a
//! blocking thread. The result is one more candidate; nothing depends on it.
//!
//! Everything that answers is on the LAN and unauthenticated. Answers are
//! parsed defensively, and a UPnP description is only fetched from the device
//! that answered the search, at a private or loopback address.

use std::{
    io::{Read, Write},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream, UdpSocket},
    sync::mpsc,
    time::{Duration, Instant},
};

pub const PORTMAP_TIMEOUT: Duration = Duration::from_millis(800);
pub const PCP_PORT: u16 = 5351;
pub const SSDP_ADDR: SocketAddr =
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(239, 255, 255, 250)), 1900);
/// Requested lifetime. Sessions longer than this lose the mapping (no renewal yet).
pub const LIFETIME_SECS: u32 = 7200;
/// PCP and NAT-PMP each get this long before the next method is tried.
const STEP: Duration = Duration::from_millis(250);
const RELEASE_TIMEOUT: Duration = Duration::from_millis(300);
const MAX_HTTP: usize = 64 * 1024;

const PCP_VERSION: u8 = 2;
const PCP_OP_MAP: u8 = 1;
const PCP_RESULT_UNSUPP_VERSION: u8 = 1;
const UDP: u8 = 17;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortMapConfig {
    /// PCP / NAT-PMP servers: normally the default gateway.
    pub gateways: Vec<IpAddr>,
    pub pcp_port: u16,
    /// Where the SSDP search goes; `None` skips UPnP.
    pub ssdp: Option<SocketAddr>,
    pub timeout: Duration,
    pub lifetime_secs: u32,
}

impl PortMapConfig {
    /// This machine's default gateways and the standard ports.
    pub fn system() -> Self {
        Self {
            gateways: default_gateways(),
            pcp_port: PCP_PORT,
            ssdp: Some(SSDP_ADDR),
            timeout: PORTMAP_TIMEOUT,
            lifetime_secs: LIFETIME_SECS,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    Pcp,
    NatPmp,
    Upnp,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct UpnpControl {
    host: SocketAddr,
    path: String,
    service: String,
}

/// A mapping the gateway granted. [`release`] it when the session ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortMapping {
    pub protocol: Protocol,
    /// Where the peer can reach us.
    pub external: SocketAddr,
    pub internal_port: u16,
    pub lifetime: Duration,
    server: SocketAddr,
    nonce: [u8; 12],
    upnp: Option<UpnpControl>,
}

/// Maps `internal_port` (UDP) through the first method that works, within
/// `cfg.timeout`. `nonce` identifies the PCP mapping (RFC 6887 §11.1): keep
/// it for the socket's lifetime, so that asking again renews the mapping
/// rather than being refused as someone else's. Draw it from a CSPRNG.
pub fn map_udp(cfg: &PortMapConfig, internal_port: u16, nonce: [u8; 12]) -> Option<PortMapping> {
    let start = Instant::now();
    let deadline = start + cfg.timeout;
    let ssdp = cfg.ssdp.map(|to| discover(to, deadline));
    for gw in cfg.gateways.iter().take(2) {
        let server = SocketAddr::new(*gw, cfg.pcp_port);
        let step = (Instant::now() + STEP).min(deadline);
        if let Some(m) = pcp_map(server, internal_port, cfg.lifetime_secs, nonce, step) {
            return Some(m);
        }
        if gw.is_ipv4() {
            let step = (Instant::now() + STEP).min(deadline);
            if let Some(m) = natpmp_map(server, internal_port, cfg.lifetime_secs, step) {
                return Some(m);
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
    }
    let found = ssdp?
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .ok()??;
    upnp_map(&found, internal_port, cfg.lifetime_secs, deadline)
}

impl PortMapping {
    /// The gateway (PCP / NAT-PMP server, or UPnP control point) that granted it.
    pub fn server(&self) -> SocketAddr {
        self.server
    }
}

/// Removes a mapping: best effort, a few hundred milliseconds at most.
pub fn release(m: &PortMapping) {
    let deadline = Instant::now() + RELEASE_TIMEOUT;
    match m.protocol {
        Protocol::Pcp => {
            let _ = pcp_request(m.server, m.internal_port, 0, m.nonce, deadline);
        }
        Protocol::NatPmp => {
            if let Ok(s) = connected(m.server, deadline) {
                let _ = s.send(&natpmp_map_request(m.internal_port, 0, 0));
            }
        }
        Protocol::Upnp => {
            if let Some(c) = &m.upnp {
                let body = format!(
                    "<NewRemoteHost></NewRemoteHost><NewExternalPort>{}</NewExternalPort><NewProtocol>UDP</NewProtocol>",
                    m.external.port()
                );
                let _ = soap(c, "DeletePortMapping", &body, deadline);
            }
        }
    }
}

fn remaining(deadline: Instant) -> Option<Duration> {
    let d = deadline.saturating_duration_since(Instant::now());
    (d >= Duration::from_millis(1)).then_some(d)
}

fn connected(server: SocketAddr, deadline: Instant) -> std::io::Result<UdpSocket> {
    let any: SocketAddr = if server.is_ipv4() {
        (Ipv4Addr::UNSPECIFIED, 0).into()
    } else {
        (Ipv6Addr::UNSPECIFIED, 0).into()
    };
    let s = UdpSocket::bind(any)?;
    s.connect(server)?;
    s.set_read_timeout(Some(
        remaining(deadline).unwrap_or(Duration::from_millis(1)),
    ))?;
    Ok(s)
}

/// Receives until `accept` takes a datagram or the deadline passes.
fn recv_until<T>(
    s: &UdpSocket,
    deadline: Instant,
    mut accept: impl FnMut(&[u8]) -> Option<T>,
) -> Option<T> {
    let mut buf = [0u8; 1100];
    loop {
        s.set_read_timeout(Some(remaining(deadline)?)).ok()?;
        match s.recv(&mut buf) {
            Ok(n) => {
                if let Some(t) = accept(&buf[..n]) {
                    return Some(t);
                }
            }
            // ICMP port unreachable on a connected socket: nobody serves there.
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => return None,
            Err(_) => return None,
        }
    }
}

fn v6_bytes(ip: IpAddr) -> [u8; 16] {
    match ip {
        IpAddr::V4(v4) => v4.to_ipv6_mapped().octets(),
        IpAddr::V6(v6) => v6.octets(),
    }
}

fn ip_of(b: &[u8]) -> IpAddr {
    let mut a = [0u8; 16];
    a.copy_from_slice(&b[..16]);
    IpAddr::V6(Ipv6Addr::from(a)).to_canonical()
}

/// RFC 6887 §7.1 + §11.1: a 60-byte MAP request.
fn pcp_map_request(client: IpAddr, internal_port: u16, lifetime: u32, nonce: [u8; 12]) -> Vec<u8> {
    let mut m = Vec::with_capacity(60);
    m.extend_from_slice(&[PCP_VERSION, PCP_OP_MAP, 0, 0]);
    m.extend_from_slice(&lifetime.to_be_bytes());
    m.extend_from_slice(&v6_bytes(client));
    m.extend_from_slice(&nonce);
    m.extend_from_slice(&[UDP, 0, 0, 0]);
    m.extend_from_slice(&internal_port.to_be_bytes());
    // Suggest the same external port; the server may pick another.
    m.extend_from_slice(&internal_port.to_be_bytes());
    m.extend_from_slice(&[0u8; 16]);
    m
}

enum PcpAnswer {
    Mapped {
        external: SocketAddr,
        lifetime: u32,
    },
    /// A NAT-PMP-only server, or another PCP version.
    Unsupported,
    Refused,
}

fn parse_pcp(b: &[u8], nonce: &[u8; 12], internal_port: u16) -> Option<PcpAnswer> {
    if b.len() >= 4 && b[0] == 0 {
        // RFC 6887 §9: a NAT-PMP server answers with its own version.
        return Some(PcpAnswer::Unsupported);
    }
    if b.len() < 24 || b[1] != 0x80 | PCP_OP_MAP {
        return None;
    }
    if b[3] == PCP_RESULT_UNSUPP_VERSION || b[0] != PCP_VERSION {
        return Some(PcpAnswer::Unsupported);
    }
    if b.len() < 60 || &b[24..36] != nonce || b[36] != UDP {
        return None;
    }
    if u16::from_be_bytes([b[40], b[41]]) != internal_port {
        return None;
    }
    if b[3] != 0 {
        return Some(PcpAnswer::Refused);
    }
    let lifetime = u32::from_be_bytes([b[4], b[5], b[6], b[7]]);
    let port = u16::from_be_bytes([b[42], b[43]]);
    let ip = ip_of(&b[44..60]);
    if port == 0 || ip.is_unspecified() || ip.is_multicast() {
        return Some(PcpAnswer::Refused);
    }
    Some(PcpAnswer::Mapped {
        external: SocketAddr::new(ip, port),
        lifetime,
    })
}

fn pcp_request(
    server: SocketAddr,
    internal_port: u16,
    lifetime: u32,
    nonce: [u8; 12],
    deadline: Instant,
) -> Option<PcpAnswer> {
    let s = connected(server, deadline).ok()?;
    let client = s.local_addr().ok()?.ip();
    let req = pcp_map_request(client, internal_port, lifetime, nonce);
    s.send(&req).ok()?;
    let resend = Instant::now() + STEP / 2;
    if let Some(a) = recv_until(&s, resend.min(deadline), |b| {
        parse_pcp(b, &nonce, internal_port)
    }) {
        return Some(a);
    }
    s.send(&req).ok()?;
    recv_until(&s, deadline, |b| parse_pcp(b, &nonce, internal_port))
}

fn pcp_map(
    server: SocketAddr,
    internal_port: u16,
    lifetime: u32,
    nonce: [u8; 12],
    deadline: Instant,
) -> Option<PortMapping> {
    match pcp_request(server, internal_port, lifetime, nonce, deadline)? {
        PcpAnswer::Mapped { external, lifetime } if lifetime > 0 => Some(PortMapping {
            protocol: Protocol::Pcp,
            external,
            internal_port,
            lifetime: Duration::from_secs(u64::from(lifetime)),
            server,
            nonce,
            upnp: None,
        }),
        _ => None,
    }
}

/// RFC 6886 §3.3: a 12-byte UDP mapping request.
fn natpmp_map_request(internal_port: u16, external_port: u16, lifetime: u32) -> Vec<u8> {
    let mut m = vec![0u8, 1, 0, 0];
    m.extend_from_slice(&internal_port.to_be_bytes());
    m.extend_from_slice(&external_port.to_be_bytes());
    m.extend_from_slice(&lifetime.to_be_bytes());
    m
}

fn natpmp_map(
    server: SocketAddr,
    internal_port: u16,
    lifetime: u32,
    deadline: Instant,
) -> Option<PortMapping> {
    let s = connected(server, deadline).ok()?;
    // External address (opcode 0) and the mapping (opcode 1), together.
    s.send(&[0, 0]).ok()?;
    s.send(&natpmp_map_request(internal_port, internal_port, lifetime))
        .ok()?;
    let mut ext_ip: Option<Ipv4Addr> = None;
    let mut mapped: Option<(u16, u32)> = None;
    recv_until(&s, deadline, |b| {
        if b.len() < 4 || b[0] != 0 || u16::from_be_bytes([b[2], b[3]]) != 0 {
            return None;
        }
        match b[1] {
            128 if b.len() >= 12 => ext_ip = Some(Ipv4Addr::new(b[8], b[9], b[10], b[11])),
            129 if b.len() >= 16 && u16::from_be_bytes([b[8], b[9]]) == internal_port => {
                mapped = Some((
                    u16::from_be_bytes([b[10], b[11]]),
                    u32::from_be_bytes([b[12], b[13], b[14], b[15]]),
                ));
            }
            _ => {}
        }
        (ext_ip.is_some() && mapped.is_some()).then_some(())
    })?;
    let (ip, (port, life)) = (ext_ip?, mapped?);
    if port == 0 || life == 0 || ip.is_unspecified() {
        return None;
    }
    Some(PortMapping {
        protocol: Protocol::NatPmp,
        external: SocketAddr::new(IpAddr::V4(ip), port),
        internal_port,
        lifetime: Duration::from_secs(u64::from(life)),
        server,
        nonce: [0; 12],
        upnp: None,
    })
}

/// A device that answered the search, and its description URL.
#[derive(Debug)]
struct Found {
    host: SocketAddr,
    path: String,
}

/// LAN-local addresses only: a UPnP answer must not send us anywhere else.
fn lan_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

/// `http://ip:port/path`, nothing else (no names, no https, no credentials).
fn parse_http_url(url: &str) -> Option<(SocketAddr, String)> {
    let rest = url.trim().strip_prefix("http://")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].to_owned()),
        None => (rest, "/".to_owned()),
    };
    let addr: SocketAddr = authority
        .parse()
        .or_else(|_| format!("{authority}:80").parse())
        .ok()?;
    if path.bytes().any(|b| b.is_ascii_control() || b == b' ') {
        return None;
    }
    Some((addr, path))
}

fn header<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    text.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

fn discover(to: SocketAddr, deadline: Instant) -> mpsc::Receiver<Option<Found>> {
    let (tx, rx) = mpsc::channel();
    let _ = std::thread::Builder::new()
        .name("ssdp".into())
        .spawn(move || {
            let _ = tx.send(discover_blocking(to, deadline));
        });
    rx
}

fn discover_blocking(to: SocketAddr, deadline: Instant) -> Option<Found> {
    let s = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    let msg = format!(
        "M-SEARCH * HTTP/1.1\r\nHOST: {to}\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\n\r\n"
    );
    s.send_to(msg.as_bytes(), to).ok()?;
    let mut buf = [0u8; 2048];
    loop {
        s.set_read_timeout(Some(remaining(deadline)?)).ok()?;
        let (n, from) = s.recv_from(&mut buf).ok()?;
        let Ok(text) = std::str::from_utf8(&buf[..n]) else {
            continue;
        };
        if !text.starts_with("HTTP/1.1 200") {
            continue;
        }
        let Some((host, path)) = header(text, "location").and_then(parse_http_url) else {
            continue;
        };
        if host.ip() != from.ip() || !lan_local(host.ip()) {
            continue;
        }
        return Some(Found { host, path });
    }
}

fn http(host: SocketAddr, request: &str, deadline: Instant) -> Option<(u16, String, SocketAddr)> {
    let mut s = TcpStream::connect_timeout(&host, remaining(deadline)?).ok()?;
    let local = s.local_addr().ok()?;
    // Before the request: macOS refuses setsockopt once the peer has closed.
    s.set_write_timeout(Some(remaining(deadline)?)).ok()?;
    s.set_read_timeout(Some(remaining(deadline)?)).ok()?;
    s.write_all(request.as_bytes()).ok()?;
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let _ = s.set_read_timeout(Some(remaining(deadline)?));
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                if out.len() > MAX_HTTP {
                    return None;
                }
                if response_complete(&out) {
                    break;
                }
            }
            Err(_) => return None,
        }
    }
    let text = String::from_utf8_lossy(&out).into_owned();
    let status: u16 = text.split_whitespace().nth(1)?.parse().ok()?;
    let body = text.split_once("\r\n\r\n").map(|(_, b)| b.to_owned())?;
    Some((status, body, local))
}

fn response_complete(raw: &[u8]) -> bool {
    let text = String::from_utf8_lossy(raw);
    let Some((head, body)) = text.split_once("\r\n\r\n") else {
        return false;
    };
    header(head, "content-length")
        .and_then(|l| l.parse::<usize>().ok())
        .is_some_and(|l| body.len() >= l)
}

fn tag<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = xml.find(&open)? + open.len();
    let end = start + xml[start..].find(&close)?;
    Some(xml[start..end].trim())
}

const WAN_SERVICES: [&str; 3] = [
    "urn:schemas-upnp-org:service:WANIPConnection:2",
    "urn:schemas-upnp-org:service:WANIPConnection:1",
    "urn:schemas-upnp-org:service:WANPPPConnection:1",
];

fn control_of(desc: &str, host: SocketAddr) -> Option<UpnpControl> {
    for want in WAN_SERVICES {
        for block in desc.split("<service>").skip(1) {
            let block = block.split("</service>").next().unwrap_or("");
            if tag(block, "serviceType") != Some(want) {
                continue;
            }
            let url = tag(block, "controlURL")?;
            let path = if url.starts_with("http://") {
                let (h, p) = parse_http_url(url)?;
                if h != host {
                    return None;
                }
                p
            } else if url.starts_with('/') {
                url.to_owned()
            } else {
                format!("/{url}")
            };
            return Some(UpnpControl {
                host,
                path,
                service: want.to_owned(),
            });
        }
    }
    None
}

fn soap(
    c: &UpnpControl,
    action: &str,
    args: &str,
    deadline: Instant,
) -> Option<(u16, String, SocketAddr)> {
    let body = format!(
        "<?xml version=\"1.0\"?>\r\n<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><u:{action} xmlns:u=\"{}\">{args}</u:{action}></s:Body></s:Envelope>\r\n",
        c.service
    );
    let req = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: text/xml; charset=\"utf-8\"\r\nSOAPAction: \"{}#{action}\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        c.path,
        c.host,
        c.service,
        body.len()
    );
    http(c.host, &req, deadline)
}

fn upnp_map(
    found: &Found,
    internal_port: u16,
    lifetime: u32,
    deadline: Instant,
) -> Option<PortMapping> {
    let get = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        found.path, found.host
    );
    let (status, desc, _) = http(found.host, &get, deadline)?;
    if status != 200 {
        return None;
    }
    let control = control_of(&desc, found.host)?;
    let (status, body, local) = soap(&control, "GetExternalIPAddress", "", deadline)?;
    if status != 200 {
        return None;
    }
    let ext: IpAddr = tag(&body, "NewExternalIPAddress")?.parse().ok()?;
    if ext.is_unspecified() || ext.is_multicast() {
        return None;
    }
    let args = format!(
        "<NewRemoteHost></NewRemoteHost><NewExternalPort>{internal_port}</NewExternalPort><NewProtocol>UDP</NewProtocol><NewInternalPort>{internal_port}</NewInternalPort><NewInternalClient>{}</NewInternalClient><NewEnabled>1</NewEnabled><NewPortMappingDescription>NexaDesk</NewPortMappingDescription><NewLeaseDuration>{lifetime}</NewLeaseDuration>",
        local.ip()
    );
    let (status, _, _) = soap(&control, "AddPortMapping", &args, deadline)?;
    if status != 200 {
        return None;
    }
    Some(PortMapping {
        protocol: Protocol::Upnp,
        external: SocketAddr::new(ext, internal_port),
        internal_port,
        lifetime: Duration::from_secs(u64::from(lifetime)),
        server: control.host,
        nonce: [0; 12],
        upnp: Some(control),
    })
}

/// The default gateways from the routing table (read only), IPv4 first.
#[cfg(target_os = "macos")]
pub fn default_gateways() -> Vec<IpAddr> {
    let mut mib = [
        libc::CTL_NET,
        libc::PF_ROUTE,
        0,
        0,
        libc::NET_RT_FLAGS,
        libc::RTF_GATEWAY,
    ];
    let mut len: libc::size_t = 0;
    // SAFETY: a size query; the pointers are valid for the call.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            std::ptr::null_mut(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || len == 0 {
        return Vec::new();
    }
    let mut buf = vec![0u8; len + 1024];
    let mut len = buf.len();
    // SAFETY: `buf` has `len` writable bytes; sysctl writes at most that many.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return Vec::new();
    }
    buf.truncate(len);
    parse_route_dump(&buf)
}

#[cfg(not(target_os = "macos"))]
pub fn default_gateways() -> Vec<IpAddr> {
    Vec::new()
}

/// Default routes' gateways from a `NET_RT_FLAGS` dump: `rt_msghdr` then the
/// sockaddrs named by `rtm_addrs`, each padded to 4 bytes.
#[cfg(target_os = "macos")]
fn parse_route_dump(buf: &[u8]) -> Vec<IpAddr> {
    const HDR: usize = std::mem::size_of::<libc::rt_msghdr>();
    // rtm_msglen (u16) at 0, rtm_addrs (i32) at 12 in `struct rt_msghdr`.
    let mut out: Vec<IpAddr> = Vec::new();
    let mut off = 0;
    while off + HDR <= buf.len() {
        let msglen = usize::from(u16::from_ne_bytes([buf[off], buf[off + 1]]));
        if msglen < HDR || off + msglen > buf.len() {
            break;
        }
        let msg = &buf[off..off + msglen];
        let addrs = i32::from_ne_bytes([msg[12], msg[13], msg[14], msg[15]]);
        let mut p = HDR;
        let (mut dst, mut gw, mut mask_zero) = (None, None, true);
        for bit in 0..8 {
            if addrs & (1 << bit) == 0 {
                continue;
            }
            if p >= msg.len() {
                break;
            }
            let sa_len = usize::from(msg[p]);
            let sa = &msg[p..(p + sa_len.max(2)).min(msg.len())];
            let ip = sockaddr_ip(sa);
            match bit {
                0 => dst = ip,
                1 => gw = ip,
                2 => mask_zero = sa.iter().skip(2).all(|b| *b == 0),
                _ => {}
            }
            p += if sa_len == 0 { 4 } else { (sa_len + 3) & !3 };
        }
        if let (Some(d), Some(g)) = (dst, gw)
            && d.is_unspecified()
            && mask_zero
            && !g.is_unspecified()
            && !out.contains(&g)
        {
            out.push(g);
        }
        off += msglen;
    }
    out.sort_by_key(|ip| ip.is_ipv6());
    out
}

#[cfg(target_os = "macos")]
fn sockaddr_ip(sa: &[u8]) -> Option<IpAddr> {
    match i32::from(*sa.get(1)?) {
        libc::AF_INET if sa.len() >= 8 => {
            Some(IpAddr::V4(Ipv4Addr::new(sa[4], sa[5], sa[6], sa[7])))
        }
        libc::AF_INET6 if sa.len() >= 24 => {
            let mut a = [0u8; 16];
            a.copy_from_slice(&sa[8..24]);
            let v6 = Ipv6Addr::from(a);
            // Link-local gateways need a scope id, which PCP over std sockets lacks.
            ((v6.segments()[0] & 0xffc0) != 0xfe80).then_some(IpAddr::V6(v6))
        }
        _ => None,
    }
}

/// Fake gateways for tests: they speak the protocols on loopback and never
/// touch a real router.
#[cfg(any(test, feature = "sim"))]
pub mod fake {
    use super::*;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering::Relaxed},
    };

    /// What a fake gateway was asked for: (internal port, lifetime). Lifetime 0 is a release.
    pub type Requests = Arc<Mutex<Vec<(u16, u32)>>>;

    /// Decides a request: `Some((external ip, port))` grants it.
    pub type Grant = Arc<dyn Fn(u16, u32) -> Option<(IpAddr, u16)> + Send + Sync>;

    pub struct FakeGateway {
        pub addr: SocketAddr,
        pub requests: Requests,
        stop: Arc<AtomicBool>,
    }

    impl Drop for FakeGateway {
        fn drop(&mut self) {
            self.stop.store(true, Relaxed);
        }
    }

    fn serve_udp(
        handle: impl Fn(&[u8]) -> Vec<Vec<u8>> + Send + 'static,
    ) -> (SocketAddr, Arc<AtomicBool>) {
        let s = UdpSocket::bind("127.0.0.1:0").unwrap();
        s.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
        let addr = s.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let st = stop.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 1100];
            while !st.load(Relaxed) {
                if let Ok((n, from)) = s.recv_from(&mut buf) {
                    for reply in handle(&buf[..n]) {
                        let _ = s.send_to(&reply, from);
                    }
                }
            }
        });
        (addr, stop)
    }

    /// A PCP server (RFC 6887 MAP only). A mapping belongs to the nonce that
    /// made it: another nonce for the same port is refused (NOT_AUTHORIZED).
    pub fn pcp(grant: Grant) -> FakeGateway {
        let requests: Requests = Arc::default();
        let r = requests.clone();
        let owners: Mutex<std::collections::HashMap<u16, Vec<u8>>> = Mutex::default();
        let (addr, stop) = serve_udp(move |b| {
            if b.len() < 60 || b[0] != PCP_VERSION || b[1] != PCP_OP_MAP {
                return Vec::new();
            }
            let lifetime = u32::from_be_bytes([b[4], b[5], b[6], b[7]]);
            let internal = u16::from_be_bytes([b[40], b[41]]);
            r.lock().unwrap().push((internal, lifetime));
            let mut out = vec![PCP_VERSION, 0x80 | PCP_OP_MAP, 0, 0];
            let mut owners = owners.lock().unwrap();
            let owned_by_other = owners.get(&internal).is_some_and(|n| n[..] != b[24..36]);
            let granted = if owned_by_other {
                None
            } else if lifetime == 0 {
                owners.remove(&internal);
                Some((IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0))
            } else {
                let g = grant(internal, lifetime);
                if g.is_some() {
                    owners.insert(internal, b[24..36].to_vec());
                }
                g
            };
            match granted {
                Some(_) => {}
                None if owned_by_other => out[3] = 2, // NOT_AUTHORIZED
                None => out[3] = 8,                   // NO_RESOURCES
            }
            out.extend_from_slice(&lifetime.to_be_bytes());
            out.extend_from_slice(&[0u8; 16]);
            out.extend_from_slice(&b[24..36]); // nonce
            out.extend_from_slice(&[UDP, 0, 0, 0]);
            out.extend_from_slice(&internal.to_be_bytes());
            let (ip, port) = granted.unwrap_or((IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0));
            out.extend_from_slice(&port.to_be_bytes());
            out.extend_from_slice(&v6_bytes(ip));
            vec![out]
        });
        FakeGateway {
            addr,
            requests,
            stop,
        }
    }

    /// A NAT-PMP-only server: answers PCP with "unsupported version".
    pub fn natpmp(grant: Grant, external_ip: Ipv4Addr) -> FakeGateway {
        let requests: Requests = Arc::default();
        let r = requests.clone();
        let (addr, stop) = serve_udp(move |b| {
            if b.first() == Some(&PCP_VERSION) {
                return vec![vec![
                    0,
                    0x80 | b.get(1).copied().unwrap_or(0),
                    0,
                    1,
                    0,
                    0,
                    0,
                    0,
                ]];
            }
            match b {
                [0, 0] => {
                    let mut out = vec![0, 128, 0, 0, 0, 0, 0, 1];
                    out.extend_from_slice(&external_ip.octets());
                    vec![out]
                }
                [0, 1, _, _, ip0, ip1, _, _, l0, l1, l2, l3] => {
                    let internal = u16::from_be_bytes([*ip0, *ip1]);
                    let lifetime = u32::from_be_bytes([*l0, *l1, *l2, *l3]);
                    r.lock().unwrap().push((internal, lifetime));
                    let (port, life) = if lifetime == 0 {
                        (0, 0)
                    } else {
                        match grant(internal, lifetime) {
                            Some((_, p)) => (p, lifetime),
                            None => return vec![vec![0, 129, 0, 4]],
                        }
                    };
                    let mut out = vec![0, 129, 0, 0, 0, 0, 0, 1];
                    out.extend_from_slice(&internal.to_be_bytes());
                    out.extend_from_slice(&port.to_be_bytes());
                    out.extend_from_slice(&life.to_be_bytes());
                    vec![out]
                }
                _ => Vec::new(),
            }
        });
        FakeGateway {
            addr,
            requests,
            stop,
        }
    }

    /// An SSDP responder and an IGD on loopback. `addr` is where to send the search.
    pub fn upnp(grant: Grant, external_ip: IpAddr) -> FakeGateway {
        let requests: Requests = Arc::default();
        let http = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        http.set_nonblocking(true).unwrap();
        let http_addr = http.local_addr().unwrap();
        let (addr, stop) = serve_udp(move |b| {
            let text = String::from_utf8_lossy(b);
            if !text.starts_with("M-SEARCH") {
                return Vec::new();
            }
            vec![format!(
                "HTTP/1.1 200 OK\r\nST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\nLOCATION: http://{http_addr}/desc.xml\r\n\r\n"
            )
            .into_bytes()]
        });
        let (r, st) = (requests.clone(), stop.clone());
        std::thread::spawn(move || {
            while !st.load(Relaxed) {
                let Ok((mut c, _)) = http.accept() else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                let _ = c.set_nonblocking(false);
                let _ = c.set_read_timeout(Some(Duration::from_millis(500)));
                let mut raw = Vec::new();
                let mut buf = [0u8; 4096];
                while !response_complete(&raw)
                    && !(raw.starts_with(b"GET") && raw.windows(4).any(|w| w == b"\r\n\r\n"))
                {
                    match c.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => raw.extend_from_slice(&buf[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&raw).into_owned();
                let body = if text.starts_with("GET /desc.xml") {
                    "<root><device><serviceList><service><serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType><controlURL>/ctl</controlURL></service></serviceList></device></root>".to_owned()
                } else if text.contains("#GetExternalIPAddress") {
                    format!(
                        "<s:Envelope><s:Body><u:GetExternalIPAddressResponse><NewExternalIPAddress>{external_ip}</NewExternalIPAddress></u:GetExternalIPAddressResponse></s:Body></s:Envelope>"
                    )
                } else if text.contains("#AddPortMapping") {
                    let port: u16 = tag(&text, "NewInternalPort")
                        .and_then(|p| p.parse().ok())
                        .unwrap_or(0);
                    let life: u32 = tag(&text, "NewLeaseDuration")
                        .and_then(|p| p.parse().ok())
                        .unwrap_or(0);
                    r.lock().unwrap().push((port, life));
                    if grant(port, life).is_none() {
                        let _ = c.write_all(
                            b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n",
                        );
                        continue;
                    }
                    "<s:Envelope><s:Body><u:AddPortMappingResponse/></s:Body></s:Envelope>"
                        .to_owned()
                } else if text.contains("#DeletePortMapping") {
                    let port: u16 = tag(&text, "NewExternalPort")
                        .and_then(|p| p.parse().ok())
                        .unwrap_or(0);
                    r.lock().unwrap().push((port, 0));
                    "<s:Envelope><s:Body><u:DeletePortMappingResponse/></s:Body></s:Envelope>"
                        .to_owned()
                } else {
                    let _ = c.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
                    continue;
                };
                let _ = c.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            }
        });
        FakeGateway {
            addr,
            requests,
            stop,
        }
    }

    /// An address nothing answers on (a gateway without the protocol).
    pub fn silent() -> FakeGateway {
        let (addr, stop) = serve_udp(|_| Vec::new());
        FakeGateway {
            addr,
            requests: Arc::default(),
            stop,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{fake::*, *};
    use std::sync::Arc;

    const NONCE: [u8; 12] = [0x5A; 12];

    fn cfg(pcp: &FakeGateway, ssdp: Option<&FakeGateway>) -> PortMapConfig {
        PortMapConfig {
            gateways: vec![pcp.addr.ip()],
            pcp_port: pcp.addr.port(),
            ssdp: ssdp.map(|g| g.addr),
            timeout: PORTMAP_TIMEOUT,
            lifetime_secs: 600,
        }
    }

    fn grant_all(ip: &str) -> Grant {
        let ip: IpAddr = ip.parse().unwrap();
        Arc::new(move |internal, _| Some((ip, internal + 1)))
    }

    #[test]
    fn pcp_maps_and_releases() {
        let gw = pcp(grant_all("198.51.100.7"));
        let m = map_udp(&cfg(&gw, None), 40_001, NONCE).expect("mapped");
        assert_eq!(m.protocol, Protocol::Pcp);
        assert_eq!(m.external, "198.51.100.7:40002".parse().unwrap());
        assert_eq!(m.lifetime, Duration::from_secs(600));
        release(&m);
        assert_eq!(
            *gw.requests.lock().unwrap(),
            vec![(40_001, 600), (40_001, 0)]
        );
    }

    #[test]
    fn asking_again_with_the_same_nonce_renews_and_another_is_refused() {
        let gw = pcp(grant_all("198.51.100.7"));
        let a = map_udp(&cfg(&gw, None), 40_050, NONCE).expect("mapped");
        let b = map_udp(&cfg(&gw, None), 40_050, NONCE).expect("renewed");
        assert_eq!((a.external, a.server()), (b.external, b.server()));
        assert!(
            map_udp(&cfg(&gw, None), 40_050, [1; 12]).is_none(),
            "someone else's"
        );
    }

    #[test]
    fn natpmp_after_pcp_says_unsupported() {
        let gw = natpmp(grant_all("198.51.100.8"), Ipv4Addr::new(198, 51, 100, 8));
        let t = Instant::now();
        let m = map_udp(&cfg(&gw, None), 40_010, NONCE).expect("mapped");
        assert!(
            t.elapsed() < Duration::from_millis(200),
            "no waiting on PCP"
        );
        assert_eq!(m.protocol, Protocol::NatPmp);
        assert_eq!(m.external, "198.51.100.8:40011".parse().unwrap());
        release(&m);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            *gw.requests.lock().unwrap(),
            vec![(40_010, 600), (40_010, 0)]
        );
    }

    #[test]
    fn upnp_when_the_gateway_has_neither_and_within_the_time_box() {
        let quiet = silent();
        let igd = upnp(grant_all("198.51.100.9"), "198.51.100.9".parse().unwrap());
        let t = Instant::now();
        let m = map_udp(&cfg(&quiet, Some(&igd)), 40_020, NONCE).expect("mapped");
        assert!(t.elapsed() <= PORTMAP_TIMEOUT + Duration::from_millis(50));
        assert_eq!(m.protocol, Protocol::Upnp);
        assert_eq!(m.external, "198.51.100.9:40020".parse().unwrap());
        release(&m);
        assert_eq!(
            *igd.requests.lock().unwrap(),
            vec![(40_020, 600), (40_020, 0)]
        );
    }

    #[test]
    fn nothing_answers_within_the_time_box() {
        let quiet = silent();
        let quiet_ssdp = silent();
        let t = Instant::now();
        assert!(map_udp(&cfg(&quiet, Some(&quiet_ssdp)), 40_030, NONCE).is_none());
        assert!(
            t.elapsed() <= PORTMAP_TIMEOUT + Duration::from_millis(50),
            "{:?}",
            t.elapsed()
        );
    }

    #[test]
    fn refusals_are_not_mappings() {
        let gw = pcp(Arc::new(|_, _| None));
        assert!(map_udp(&cfg(&gw, None), 40_040, NONCE).is_none());
    }

    #[test]
    fn upnp_answers_must_stay_on_the_lan() {
        assert!(parse_http_url("https://192.168.1.1/x").is_none());
        assert!(parse_http_url("http://user@192.168.1.1/x").is_none());
        assert!(parse_http_url("http://router.example/x").is_none());
        assert_eq!(
            parse_http_url("http://192.168.1.1:5000/rootDesc.xml"),
            Some(("192.168.1.1:5000".parse().unwrap(), "/rootDesc.xml".into()))
        );
        assert!(lan_local("192.168.1.1".parse().unwrap()));
        assert!(!lan_local("203.0.113.1".parse().unwrap()));
        let host: SocketAddr = "192.168.1.1:5000".parse().unwrap();
        let desc = "<service><serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType><controlURL>http://203.0.113.1/ctl</controlURL></service>";
        assert!(control_of(desc, host).is_none(), "control elsewhere");
        let desc = "<service><serviceType>urn:schemas-upnp-org:service:WANPPPConnection:1</serviceType><controlURL>ctl/ppp</controlURL></service>";
        assert_eq!(control_of(desc, host).unwrap().path, "/ctl/ppp");
    }

    #[test]
    fn pcp_parser_survives_garbage() {
        let nonce = [7u8; 12];
        let mut x = 0x1234_5678u64;
        for _ in 0..20_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let len = (x % 80) as usize;
            let mut b: Vec<u8> = (0..len).map(|i| (x >> (i % 56)) as u8).collect();
            if b.len() > 2 {
                b[0] = 2;
                b[1] = 0x81;
            }
            let _ = parse_pcp(&b, &nonce, 1);
        }
    }

    #[test]
    fn reading_the_route_table_does_not_fail() {
        let _ = default_gateways();
    }
}
