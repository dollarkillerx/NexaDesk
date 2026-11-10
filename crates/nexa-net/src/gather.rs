//! Candidate gathering helpers (§4.2): which local addresses to offer, and what
//! two STUN answers say about the NAT. The I/O lives with the session.
//!
//! Port mapping is in [`crate::portmap`], the Tailscale LocalAPI in
//! [`crate::tailscale`]. Not implemented: mDNS. Tailnet addresses are
//! recognised by range on any interface.

use std::net::{IpAddr, SocketAddr};

use crate::{
    check::{Families, PeerNat, is_tailnet},
    path::CandKind,
    stun::{NatClass, classify_nat},
};

/// Interfaces whose addresses are never worth offering: Apple's peer-to-peer
/// links, hypervisor and container bridges, and tunnels other than Tailscale's.
const VIRTUAL_PREFIXES: &[&str] = &[
    "awdl", "llw", "bridge", "vmnet", "vboxnet", "docker", "veth", "br-", "virbr", "anpi", "gif",
    "stf", "ap1", "utun", "ipsec", "zt",
];

/// How a local address should be offered, or `None` to leave it out.
pub fn classify_local(ifname: &str, ip: IpAddr) -> Option<CandKind> {
    if ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() {
        return None;
    }
    // Tailscale's interface is a utun on macOS: decide by range before the name check.
    if is_tailnet(ip) {
        return Some(CandKind::TailnetDirect);
    }
    if VIRTUAL_PREFIXES.iter().any(|p| ifname.starts_with(p)) {
        return None;
    }
    match ip {
        IpAddr::V4(v4) => (!v4.is_link_local() && !v4.is_broadcast()).then_some(CandKind::Host),
        IpAddr::V6(v6) => {
            let link_local = (v6.segments()[0] & 0xffc0) == 0xfe80;
            (!link_local && v6.to_ipv4_mapped().is_none()).then_some(CandKind::Ipv6)
        }
    }
}

/// This machine's interface addresses. Never fails: an error yields none.
pub fn system_addresses() -> Vec<(String, IpAddr)> {
    if_addrs::get_if_addrs()
        .map(|list| list.into_iter().map(|i| (i.name.clone(), i.ip())).collect())
        .unwrap_or_default()
}

/// Host and Tailnet candidates for a socket bound to `port`, in one family.
pub fn local_candidates(
    addrs: &[(String, IpAddr)],
    port: u16,
    family_v4: bool,
) -> Vec<(SocketAddr, CandKind)> {
    local_candidates_for(addrs, port, Families::from(family_v4))
}

/// Host, IPv6 and Tailnet candidates for a socket bound to `port`; IPv4 first.
pub fn local_candidates_for(
    addrs: &[(String, IpAddr)],
    port: u16,
    families: Families,
) -> Vec<(SocketAddr, CandKind)> {
    let mut out: Vec<(SocketAddr, CandKind)> = Vec::new();
    let mut sorted: Vec<&(String, IpAddr)> = addrs.iter().collect();
    sorted.sort_by_key(|(_, ip)| ip.is_ipv6());
    for (name, ip) in sorted {
        if !families.allows(SocketAddr::new(*ip, port)) {
            continue;
        }
        if let Some(kind) = classify_local(name, *ip) {
            let a = SocketAddr::new(*ip, port);
            if !out.iter().any(|(x, _)| *x == a) {
                out.push((a, kind));
            }
        }
    }
    out
}

/// §4.2: two different servers' view of our mapping. One answer cannot tell.
pub fn nat_from_mappings(mappings: &[SocketAddr]) -> PeerNat {
    match mappings {
        [a, b, ..] if a.is_ipv4() == b.is_ipv4() => match classify_nat(*a, *b) {
            NatClass::Easy => PeerNat::Easy,
            NatClass::Hard { port_delta } => PeerNat::Hard { port_delta },
        },
        _ => PeerNat::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn classifies_interfaces() {
        assert_eq!(
            classify_local("en0", ip("192.168.1.20")),
            Some(CandKind::Host)
        );
        assert_eq!(
            classify_local("en0", ip("203.0.113.5")),
            Some(CandKind::Host)
        );
        assert_eq!(
            classify_local("utun4", ip("100.101.102.103")),
            Some(CandKind::TailnetDirect)
        );
        assert_eq!(
            classify_local("utun4", ip("fd7a:115c:a1e0::1")),
            Some(CandKind::TailnetDirect)
        );
        assert_eq!(
            classify_local("utun2", ip("10.8.0.2")),
            None,
            "other VPN tunnels"
        );
        assert_eq!(classify_local("lo0", ip("127.0.0.1")), None);
        assert_eq!(classify_local("en0", ip("169.254.3.4")), None);
        assert_eq!(classify_local("en0", ip("fe80::1")), None);
        assert_eq!(classify_local("awdl0", ip("fe80::2")), None);
        assert_eq!(classify_local("bridge100", ip("192.168.64.1")), None);
        assert_eq!(
            classify_local("en0", ip("2001:db8::5")),
            Some(CandKind::Ipv6)
        );
        assert_eq!(
            classify_local("en0", ip("100.63.0.1")),
            Some(CandKind::Host),
            "outside 100.64/10"
        );
    }

    #[test]
    fn local_candidates_dedupe_and_filter_family() {
        let addrs = vec![
            ("en0".to_owned(), ip("192.168.1.20")),
            ("en1".to_owned(), ip("192.168.1.20")),
            ("en0".to_owned(), ip("2001:db8::5")),
            ("lo0".to_owned(), ip("127.0.0.1")),
        ];
        let v4 = local_candidates(&addrs, 4000, true);
        assert_eq!(
            v4,
            vec![("192.168.1.20:4000".parse().unwrap(), CandKind::Host)]
        );
        assert_eq!(local_candidates(&addrs, 4000, false).len(), 1);
        let both = local_candidates_for(&addrs, 4000, Families::DUAL);
        assert_eq!(
            both,
            vec![
                ("192.168.1.20:4000".parse().unwrap(), CandKind::Host),
                ("[2001:db8::5]:4000".parse().unwrap(), CandKind::Ipv6),
            ]
        );
    }

    #[test]
    fn nat_needs_two_answers() {
        let a: SocketAddr = "198.51.100.1:5000".parse().unwrap();
        let b: SocketAddr = "198.51.100.1:5003".parse().unwrap();
        assert_eq!(nat_from_mappings(&[a]), PeerNat::Unknown);
        assert_eq!(nat_from_mappings(&[a, a]), PeerNat::Easy);
        assert_eq!(nat_from_mappings(&[a, b]), PeerNat::Hard { port_delta: 3 });
    }

    #[test]
    fn system_addresses_do_not_fail() {
        let _ = system_addresses();
    }
}
