//! Server configuration: a TOML file (see `nexa-signal --example-config`).

use std::{net::SocketAddr, path::PathBuf, time::Duration};

use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("config file: {0}")]
    Read(String),
    #[error("config: {0}")]
    Parse(String),
    #[error("config: {0}")]
    Invalid(String),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub signal: SignalSection,
    #[serde(default)]
    pub stun: StunSection,
    #[serde(default)]
    pub relay: Option<RelaySection>,
    #[serde(default)]
    pub limits: Limits,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignalSection {
    /// TCP address for WebSocket signaling.
    pub listen: SocketAddr,
    /// PEM certificate chain and key. Without them the server speaks plain `ws://`,
    /// which is only sensible on loopback or behind a TLS-terminating proxy.
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
    /// Host names clients dial. When set, a registration made for another
    /// server is refused.
    #[serde(default)]
    pub server_names: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StunSection {
    /// One or two UDP addresses. Two, on different IPs, let clients measure
    /// whether their NAT's mapping depends on the destination (§4.2).
    #[serde(default)]
    pub listen: Vec<SocketAddr>,
    /// What clients are told, in the same order. Defaults to `listen`.
    #[serde(default)]
    pub advertise: Vec<SocketAddr>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelaySection {
    pub listen: SocketAddr,
    pub advertise: Option<SocketAddr>,
    /// Per-session forwarding limit, bits per second.
    #[serde(default = "default_relay_bps")]
    pub bandwidth_bps: u64,
    #[serde(default = "default_relay_sessions")]
    pub max_sessions: usize,
    /// How long a relay token may be used to bind.
    #[serde(default = "default_token_ttl")]
    pub token_ttl_secs: u64,
}

fn default_relay_bps() -> u64 {
    50_000_000
}
fn default_relay_sessions() -> usize {
    1000
}
fn default_token_ttl() -> u64 {
    3600
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Limits {
    pub max_connections: usize,
    pub max_connections_per_ip: usize,
    /// Registrations per IP: rate per second and burst.
    pub register_per_ip: (f64, f64),
    /// ID lookups per IP, which bounds how fast IDs can be scanned.
    pub lookup_per_ip: (f64, f64),
    /// Connection requests per IP, per requesting ID, and per target ID.
    pub connect_per_ip: (f64, f64),
    pub connect_per_source: (f64, f64),
    pub connect_per_target: (f64, f64),
    /// Messages of any kind per connection.
    pub messages_per_conn: (f64, f64),
    /// STUN answers per source IP.
    pub stun_per_ip: (f64, f64),
    pub max_sessions_per_conn: usize,
    pub session_ttl_secs: u64,
    pub trickles_per_session: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_connections: 10_000,
            max_connections_per_ip: 32,
            register_per_ip: (0.5, 10.0),
            lookup_per_ip: (1.0, 20.0),
            connect_per_ip: (1.0, 10.0),
            connect_per_source: (1.0, 10.0),
            connect_per_target: (0.5, 6.0),
            messages_per_conn: (20.0, 60.0),
            stun_per_ip: (20.0, 60.0),
            max_sessions_per_conn: 16,
            session_ttl_secs: 60,
            trickles_per_session: 16,
        }
    }
}

impl Limits {
    pub fn session_ttl(&self) -> Duration {
        Duration::from_secs(self.session_ttl_secs.max(5))
    }
}

pub const EXAMPLE: &str = r#"# nexa-signal: signaling, STUN and relay in one process (§4.3, §4.6).
# All state is in memory; restarting makes clients register again.

[signal]
listen = "0.0.0.0:443"
tls_cert = "/etc/nexa-signal/fullchain.pem"
tls_key = "/etc/nexa-signal/privkey.pem"
# The names clients use to reach this server (recommended).
server_names = ["signal.example.com"]

[stun]
# Two addresses, ideally on two different public IPs, so clients can tell an
# easy NAT from a hard one. With one, or two on the same IP, classification
# degrades and the server says so at start-up and to every client.
listen = ["0.0.0.0:3478", "0.0.0.0:3479"]
advertise = ["203.0.113.10:3478", "203.0.113.10:3479"]

[relay]
listen = "0.0.0.0:3480"
advertise = "203.0.113.10:3480"
bandwidth_bps = 50000000
max_sessions = 1000
token_ttl_secs = 3600

[limits]
max_connections_per_ip = 32
"#;

impl Config {
    pub fn from_toml(text: &str) -> Result<Self, ConfigError> {
        let c: Config = toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;
        c.validate()?;
        Ok(c)
    }

    pub fn load(path: &std::path::Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| ConfigError::Read(format!("{}: {e}", path.display())))?;
        Self::from_toml(&text)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.signal.tls_cert.is_some() != self.signal.tls_key.is_some() {
            return Err(ConfigError::Invalid(
                "tls_cert and tls_key go together".into(),
            ));
        }
        if self.stun.listen.len() > 2 {
            return Err(ConfigError::Invalid("at most two STUN addresses".into()));
        }
        if !self.stun.advertise.is_empty() && self.stun.advertise.len() != self.stun.listen.len() {
            return Err(ConfigError::Invalid(
                "stun.advertise must list one address per stun.listen".into(),
            ));
        }
        for a in self.stun_advertised() {
            if a.ip().is_unspecified() {
                return Err(ConfigError::Invalid(format!(
                    "STUN listens on {a}; set stun.advertise to the address clients reach"
                )));
            }
        }
        if let Some(r) = &self.relay
            && self
                .relay_advertised()
                .is_some_and(|a| a.ip().is_unspecified())
        {
            return Err(ConfigError::Invalid(format!(
                "relay listens on {}; set relay.advertise to the address clients reach",
                r.listen
            )));
        }
        Ok(())
    }

    pub fn stun_advertised(&self) -> Vec<SocketAddr> {
        if self.stun.advertise.is_empty() {
            self.stun.listen.clone()
        } else {
            self.stun.advertise.clone()
        }
    }

    pub fn relay_advertised(&self) -> Option<SocketAddr> {
        self.relay.as_ref().map(|r| r.advertise.unwrap_or(r.listen))
    }

    /// Why clients cannot fully classify their NAT with this configuration.
    pub fn stun_note(&self) -> Option<String> {
        stun_note(&self.stun_advertised())
    }
}

pub fn stun_note(advertised: &[SocketAddr]) -> Option<String> {
    match advertised {
        [] => Some("no STUN service: clients cannot learn their public address, so hole punching is limited to what the peer discovers".into()),
        [_] => Some("one STUN address: clients cannot tell an easy NAT from a hard one, and port prediction is off".into()),
        [a, b, ..] if a.ip() == b.ip() => Some("both STUN addresses share one IP: NATs whose mapping depends only on the destination address will look easy".into()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_parses_and_validates() {
        let c = Config::from_toml(EXAMPLE).unwrap();
        assert_eq!(c.stun_advertised().len(), 2);
        assert!(c.stun_note().unwrap().contains("share one IP"));
        assert_eq!(c.relay_advertised().unwrap().port(), 3480);
    }

    #[test]
    fn rejects_unusable_configs() {
        let wildcard = r#"
[signal]
listen = "127.0.0.1:8443"
[stun]
listen = ["0.0.0.0:3478"]
"#;
        assert!(Config::from_toml(wildcard).is_err());
        let half_tls = r#"
[signal]
listen = "127.0.0.1:8443"
tls_cert = "a.pem"
"#;
        assert!(Config::from_toml(half_tls).is_err());
        let unknown = r#"
[signal]
listen = "127.0.0.1:8443"
lisen = "x"
"#;
        assert!(Config::from_toml(unknown).is_err());
    }

    #[test]
    fn notes_degraded_classification() {
        let a: SocketAddr = "203.0.113.1:3478".parse().unwrap();
        let b: SocketAddr = "203.0.113.2:3478".parse().unwrap();
        assert!(stun_note(&[a]).is_some());
        assert!(stun_note(&[a, b]).is_none());
        assert!(stun_note(&[]).is_some());
    }
}
