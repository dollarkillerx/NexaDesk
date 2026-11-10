//! LAN discovery over mDNS / DNS-SD (§4.11).
//!
//! A host announces `_nexadesk._udp.local.` with its listening port; viewers on the
//! same link list what they hear. What is heard is a **hint**, never trusted: it
//! only fills in an address to dial. Who answers there is still checked by TLS and
//! pairing exactly as for an address typed by hand, so a forged announcement can at
//! worst offer a connection that then fails.
//!
//! The instance name is the device fingerprint (`DeviceId`, 32 hex digits from the
//! device key), which is unique, so two machines with the same name do not collide;
//! the name shown comes from the TXT record. The nine-digit ID is not used: it is
//! assigned by a signaling server, and a LAN-only host has none.

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    thread::JoinHandle,
};

use mdns_sd::{ResolvedService, ServiceDaemon, ServiceEvent, ServiceInfo};
use thiserror::Error;

pub const SERVICE_TYPE: &str = "_nexadesk._udp.local.";

/// TXT keys. `v` is the record format, not the protocol version.
const TXT_VERSION: &str = "v";
const TXT_ID: &str = "id";
const TXT_NAME: &str = "n";
const TXT_OS: &str = "os";
const RECORD_VERSION: &str = "1";

/// `DeviceId` in lower-case hex (§9.1).
const ID_LEN: usize = 32;
const MAX_NAME_CHARS: usize = 64;
const MAX_OS_CHARS: usize = 16;
/// Bounds the table a noisy link can grow.
const MAX_PEERS: usize = 256;

#[derive(Debug, Error)]
#[error("LAN discovery: {0}")]
pub struct DiscoveryError(String);

impl From<mdns_sd::Error> for DiscoveryError {
    fn from(e: mdns_sd::Error) -> Self {
        Self(e.to_string())
    }
}

/// What a host announces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Advert {
    pub id: String,
    pub name: String,
    pub os: String,
    pub port: u16,
    /// The one address to announce when the host listens on a single interface.
    /// `None` announces every address of every interface, following changes.
    pub ip: Option<IpAddr>,
}

/// A host heard on the LAN.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LanPeer {
    pub id: String,
    pub name: String,
    pub os: String,
    /// Best first: IPv4 before IPv6. Never empty.
    pub addrs: Vec<SocketAddr>,
}

impl LanPeer {
    /// Whether every address it announced is one of `local`: the host on this very
    /// machine, which is not worth listing as somewhere to connect to.
    pub fn is_this_machine(&self, local: &[IpAddr]) -> bool {
        self.addrs
            .iter()
            .all(|a| local.contains(&a.ip().to_canonical()))
    }
}

/// This machine's interface addresses, for [`LanPeer::is_this_machine`].
pub fn local_ips() -> Vec<IpAddr> {
    if_addrs::get_if_addrs()
        .map(|list| list.into_iter().map(|i| i.ip().to_canonical()).collect())
        .unwrap_or_default()
}

/// Announces this host for as long as it lives.
pub struct Advertiser {
    daemon: ServiceDaemon,
    fullname: String,
}

impl Advertiser {
    pub fn start(advert: &Advert) -> Result<Self, DiscoveryError> {
        if !valid_id(&advert.id) {
            return Err(DiscoveryError("not a device fingerprint".into()));
        }
        let daemon = ServiceDaemon::new()?;
        let name = clean(&advert.name, MAX_NAME_CHARS);
        let os = clean(&advert.os, MAX_OS_CHARS);
        let txt = [
            (TXT_VERSION, RECORD_VERSION),
            (TXT_ID, advert.id.as_str()),
            (TXT_NAME, name.as_str()),
            (TXT_OS, os.as_str()),
        ];
        let host = format!("nexa-{}.local.", advert.id);
        let info = match advert.ip {
            Some(ip) => {
                ServiceInfo::new(SERVICE_TYPE, &advert.id, &host, ip, advert.port, &txt[..])?
            }
            // Addresses follow the interfaces as they come and go.
            None => ServiceInfo::new(SERVICE_TYPE, &advert.id, &host, "", advert.port, &txt[..])?
                .enable_addr_auto(),
        };
        let fullname = info.get_fullname().to_string();
        daemon.register(info)?;
        Ok(Self { daemon, fullname })
    }

    #[cfg(test)]
    fn daemon(&self) -> &ServiceDaemon {
        &self.daemon
    }
}

impl Drop for Advertiser {
    fn drop(&mut self) {
        // Sends the goodbye so viewers drop the entry at once. Not waited for.
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}

/// Listens for hosts until dropped.
pub struct Browser {
    daemon: ServiceDaemon,
    peers: Arc<Mutex<HashMap<String, LanPeer>>>,
    thread: Option<JoinHandle<()>>,
}

impl Browser {
    pub fn start() -> Result<Self, DiscoveryError> {
        Self::with_daemon(ServiceDaemon::new()?)
    }

    fn with_daemon(daemon: ServiceDaemon) -> Result<Self, DiscoveryError> {
        let events = daemon.browse(SERVICE_TYPE)?;
        let peers: Arc<Mutex<HashMap<String, LanPeer>>> = Arc::default();
        let table = peers.clone();
        let thread = std::thread::Builder::new()
            .name("lan-discovery".into())
            .spawn(move || {
                // Ends when the daemon shuts down and drops the sender.
                while let Ok(event) = events.recv() {
                    let mut t = table.lock().unwrap_or_else(|e| e.into_inner());
                    match event {
                        ServiceEvent::ServiceResolved(s) => match peer_of(&s) {
                            Some(p) if t.len() < MAX_PEERS || t.contains_key(&s.fullname) => {
                                t.insert(s.fullname.clone(), p);
                            }
                            Some(_) => {}
                            None => {
                                t.remove(&s.fullname);
                            }
                        },
                        ServiceEvent::ServiceRemoved(_, fullname) => {
                            t.remove(&fullname);
                        }
                        _ => {}
                    }
                }
            })
            .map_err(|e| DiscoveryError(e.to_string()))?;
        Ok(Self {
            daemon,
            peers,
            thread: Some(thread),
        })
    }

    /// The hosts heard so far, sorted by name, then ID.
    pub fn peers(&self) -> Vec<LanPeer> {
        let mut list: Vec<LanPeer> = self
            .peers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        list.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.id.cmp(&b.id))
        });
        list
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.daemon.stop_browse(SERVICE_TYPE);
        let _ = self.daemon.shutdown();
        // Not joined: shutdown is asynchronous and the thread holds nothing.
        drop(self.thread.take());
    }
}

fn peer_of(s: &ResolvedService) -> Option<LanPeer> {
    peer_from(s.port, s.addresses.iter().map(|a| a.to_ip_addr()), |k| {
        s.get_property_val_str(k)
    })
}

/// Validates a resolved record. `None` for anything that is not a well-formed
/// NexaDesk announcement.
fn peer_from<'a>(
    port: u16,
    addrs: impl Iterator<Item = IpAddr>,
    txt: impl Fn(&str) -> Option<&'a str>,
) -> Option<LanPeer> {
    if txt(TXT_VERSION)? != RECORD_VERSION || port == 0 {
        return None;
    }
    let id = txt(TXT_ID)?;
    if !valid_id(id) {
        return None;
    }
    let mut addrs: Vec<SocketAddr> = addrs
        .filter(|ip| usable(*ip))
        .map(|ip| SocketAddr::new(ip, port))
        .collect();
    addrs.sort_by_key(|a| (a.is_ipv6(), a.ip()));
    addrs.dedup();
    if addrs.is_empty() {
        return None;
    }
    let name = clean(txt(TXT_NAME).unwrap_or(""), MAX_NAME_CHARS);
    Some(LanPeer {
        id: id.to_string(),
        name: if name.is_empty() {
            id.to_string()
        } else {
            name
        },
        os: clean(txt(TXT_OS).unwrap_or(""), MAX_OS_CHARS),
        addrs,
    })
}

fn valid_id(id: &str) -> bool {
    id.len() == ID_LEN && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Addresses worth dialling. IPv6 link-local needs an interface scope that a
/// `SocketAddr` built here would not carry, so it is left out; so is anything that
/// cannot be a host on the link.
fn usable(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v4) => !(v4.is_unspecified() || v4.is_multicast() || v4.is_broadcast()),
        IpAddr::V6(v6) => {
            !(v6.is_unspecified() || v6.is_multicast() || (v6.segments()[0] & 0xffc0) == 0xfe80)
        }
    }
}

/// Text from the network, made safe to show: no control or bidi-override
/// characters, trimmed, bounded.
fn clean(s: &str, max: usize) -> String {
    s.chars()
        .filter(|c| {
            !c.is_control() && !matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
        })
        .take(max)
        .collect::<String>()
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    const ID: &str = "0123456789abcdef0123456789abcdef";

    fn txt<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<&'a str> {
        move |k| pairs.iter().find(|(key, _)| *key == k).map(|(_, v)| *v)
    }

    fn ips(list: &[&str]) -> Vec<IpAddr> {
        list.iter().map(|s| s.parse().unwrap()).collect()
    }

    #[test]
    fn a_good_record_becomes_a_peer_with_ipv4_first() {
        let p = peer_from(
            21118,
            ips(&["fe80::1", "2001:db8::5", "192.168.1.20"]).into_iter(),
            txt(&[("v", "1"), ("id", ID), ("n", "Studio Mac"), ("os", "macos")]),
        )
        .unwrap();
        assert_eq!(p.id, ID);
        assert_eq!(p.name, "Studio Mac");
        assert_eq!(p.os, "macos");
        assert_eq!(
            p.addrs,
            vec![
                "192.168.1.20:21118".parse().unwrap(),
                "[2001:db8::5]:21118".parse().unwrap()
            ]
        );
    }

    #[test]
    fn malformed_records_are_ignored() {
        let addr = || ips(&["192.168.1.20"]).into_iter();
        let ok = [("v", "1"), ("id", ID)];
        assert!(peer_from(1, addr(), txt(&ok)).is_some());
        assert!(peer_from(0, addr(), txt(&ok)).is_none(), "port 0");
        assert!(peer_from(1, addr(), txt(&[("v", "2"), ("id", ID)])).is_none());
        assert!(peer_from(1, addr(), txt(&[("id", ID)])).is_none());
        assert!(peer_from(1, addr(), txt(&[("v", "1"), ("id", &ID[..31])])).is_none());
        assert!(
            peer_from(
                1,
                addr(),
                txt(&[("v", "1"), ("id", "0123456789abcdef0123456789abcdeX")])
            )
            .is_none()
        );
        assert!(peer_from(1, addr(), txt(&[("v", "1")])).is_none());
        let only_link_local = ips(&["fe80::1", "224.0.0.251", "0.0.0.0"]).into_iter();
        assert!(peer_from(1, only_link_local, txt(&ok)).is_none());
    }

    #[test]
    fn names_from_the_network_are_cleaned() {
        let p = peer_from(
            1,
            ips(&["10.0.0.2"]).into_iter(),
            txt(&[
                ("v", "1"),
                ("id", ID),
                ("n", "  evil\u{202E}txt\n\u{1b}[2J "),
                ("os", "macos-with-a-very-long-suffix"),
            ]),
        )
        .unwrap();
        assert_eq!(p.name, "eviltxt[2J");
        assert_eq!(p.os.chars().count(), MAX_OS_CHARS);
        let unnamed = peer_from(
            1,
            ips(&["10.0.0.2"]).into_iter(),
            txt(&[("v", "1"), ("id", ID), ("n", "\n")]),
        )
        .unwrap();
        assert_eq!(unnamed.name, ID);
        assert_eq!(
            clean(&"a".repeat(500), MAX_NAME_CHARS).len(),
            MAX_NAME_CHARS
        );
    }

    #[test]
    fn the_host_on_this_machine_is_recognised() {
        let p = peer_from(
            1,
            ips(&["192.168.1.20", "2001:db8::5"]).into_iter(),
            txt(&[("v", "1"), ("id", ID)]),
        )
        .unwrap();
        assert!(p.is_this_machine(&ips(&["127.0.0.1", "192.168.1.20", "2001:db8::5"])));
        assert!(!p.is_this_machine(&ips(&["192.168.1.20"])));
        assert!(!local_ips().is_empty(), "loopback at least");
    }

    #[test]
    fn an_advert_needs_a_device_id() {
        let bad = Advert {
            id: "abc".into(),
            name: "x".into(),
            os: "macos".into(),
            port: 1,
            ip: None,
        };
        assert!(Advertiser::start(&bad).is_err());
    }

    /// Real multicast on this machine's interfaces. Ignored by default: it needs a
    /// network that carries multicast and, on macOS, Local Network permission.
    #[test]
    #[ignore]
    fn a_host_is_heard_and_then_forgotten() {
        let advert = Advert {
            id: "fedcba9876543210fedcba9876543210".into(),
            name: "Test Host".into(),
            os: "macos".into(),
            port: 40123,
            ip: None,
        };
        let host = Advertiser::start(&advert).unwrap();
        host.daemon().set_multicast_loop_v4(true).unwrap();
        let daemon = ServiceDaemon::new().unwrap();
        daemon.set_multicast_loop_v4(true).unwrap();
        let viewer = Browser::with_daemon(daemon).unwrap();
        let heard = |want: bool| {
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                let found = viewer.peers().iter().any(|p| p.id == advert.id);
                if found == want {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            false
        };
        assert!(heard(true), "announcement not heard");
        let p = viewer
            .peers()
            .into_iter()
            .find(|p| p.id == advert.id)
            .unwrap();
        assert_eq!(p.name, "Test Host");
        assert!(p.addrs.iter().all(|a| a.port() == 40123));
        drop(host);
        assert!(heard(false), "goodbye not heard");
    }
}
