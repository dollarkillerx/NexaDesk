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
    pub web: Option<WebSection>,
    #[serde(default)]
    pub limits: Limits,
}

/// Browser access across the internet (docs/WEB_CLIENT.md §12): the page on the
/// signaling port, and TURN on the first STUN address.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebSection {
    /// A password for browsers, apart from the signaling key (browsers cannot hold
    /// that one): the page asks for it next to the device ID, and the server puts a
    /// browser through, and hands it TURN credentials, only when it is right. Without
    /// one, browsers are not asked for anything here; what lets a browser into a host
    /// is still the host's own password, one-time code or consent (§9).
    #[serde(default)]
    pub password: Option<String>,
    /// TURN allocations at once. Each browser session takes two.
    #[serde(default = "default_turn_allocations")]
    pub max_allocations: usize,
    #[serde(default = "default_turn_per_ip")]
    pub max_per_ip: usize,
}

impl std::fmt::Debug for WebSection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebSection")
            .field("password", &self.password.as_ref().map(|_| ".."))
            .field("max_allocations", &self.max_allocations)
            .field("max_per_ip", &self.max_per_ip)
            .finish()
    }
}

impl std::fmt::Debug for SignalSection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignalSection")
            .field("listen", &self.listen)
            .field("tls_cert", &self.tls_cert)
            .field("tls_key", &self.tls_key)
            .field("server_names", &self.server_names)
            .field(
                "access_secrets",
                &format!("{} (hidden)", self.access_secrets.len()),
            )
            .finish()
    }
}

fn default_turn_allocations() -> usize {
    200
}
fn default_turn_per_ip() -> usize {
    8
}

#[derive(Clone, Deserialize)]
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
    /// The signaling key, required: a device registers only by proving it knows one
    /// of these, and several let a key be replaced without cutting everyone off at
    /// once (list the new one, hand it out, then drop the old). The key never crosses
    /// the wire; see `nexa_auth::signal_access`. It gates the signaling, the relays
    /// that registered devices are granted and the lookups, but not STUN, and not the
    /// browser page and TURN (`[web]`, which has a password of its own).
    #[serde(default)]
    pub access_secrets: Vec<String>,
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

/// The shortest signaling key accepted.
pub const MIN_ACCESS_SECRET_CHARS: usize = 10;
/// The key `--example-config` prints: refused, so that nobody runs with it.
/// What the page's password field takes (its `maxlength`).
pub const MAX_WEB_PASSWORD_CHARS: usize = 128;
pub const EXAMPLE_SECRET: &str = "CHANGE-THIS-to-a-long-key-of-your-own";

pub const EXAMPLE: &str = r#"# nexa-signal: signaling, STUN and relay in one process (§4.3, §4.6).
# All state is in memory; restarting makes clients register again.

[signal]
listen = "0.0.0.0:443"
tls_cert = "/etc/nexa-signal/fullchain.pem"
tls_key = "/etc/nexa-signal/privkey.pem"
# The names clients use to reach this server (recommended).
server_names = ["signal.example.com"]
# The signaling key, required. Devices register only by proving they know it, and it
# keeps strangers off the relays. Clients enter the same key in their settings; it is
# never sent, only a proof of it. At least 10 characters. List two to replace one
# without cutting everyone off. Keep this file readable by the service only.
access_secrets = ["CHANGE-THIS-to-a-long-key-of-your-own"]

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

[web]
# Browsers anywhere: the page at https://signal.example.com/, and TURN on the first
# STUN address. A browser session's screen and input go through this server.
# A password for browsers (optional, and not the signaling key): the page asks for it
# next to the device ID. Without it browsers are asked for nothing here.
# password = "something people will type"
max_allocations = 200

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
        // A signaling server always has a key: the only thing between its relays and
        // the whole internet.
        if self.signal.access_secrets.is_empty() {
            return Err(ConfigError::Invalid(
                "signal.access_secrets: the signaling server needs a key; list at least one (see --example-config)"
                    .into(),
            ));
        }
        for s in &self.signal.access_secrets {
            // Clients trim what a person typed, so a key with spaces at either end
            // could never be matched.
            if s.trim() != s {
                return Err(ConfigError::Invalid(
                    "signal.access_secrets: a key may not start or end with a space or a line break"
                        .into(),
                ));
            }
            if s.chars().count() < MIN_ACCESS_SECRET_CHARS {
                return Err(ConfigError::Invalid(format!(
                    "signal.access_secrets: a key needs at least {MIN_ACCESS_SECRET_CHARS} characters"
                )));
            }
            if s == EXAMPLE_SECRET {
                return Err(ConfigError::Invalid(
                    "signal.access_secrets: replace the key of the example with your own".into(),
                ));
            }
        }
        if let Some(w) = &self.web
            && let Some(p) = &w.password
            && (p.is_empty() || p.trim() != p || p.chars().count() > MAX_WEB_PASSWORD_CHARS)
        {
            return Err(ConfigError::Invalid(
                "web.password: not empty, at most 128 characters, and no space or line break at either end (leave it out for browsers without a password)"
                    .into(),
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
        if self.web.is_some() && self.stun.listen.is_empty() {
            return Err(ConfigError::Invalid(
                "[web] needs a STUN address: TURN runs on the first one".into(),
            ));
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

    const KEY: &str = r#"access_secrets = ["a key of its own"]"#;

    /// `--example-config` with a key of one's own, as an operator would make it.
    fn example() -> String {
        EXAMPLE.replace(&format!("{EXAMPLE_SECRET:?}"), "\"a key of its own\"")
    }

    #[test]
    fn example_parses_and_validates_once_it_has_a_key_of_its_own() {
        let c = Config::from_toml(&example()).unwrap();
        assert_eq!(c.stun_advertised().len(), 2);
        assert!(c.stun_note().unwrap().contains("share one IP"));
        assert_eq!(c.relay_advertised().unwrap().port(), 3480);
        // As printed it is refused: nobody runs with the key of the example.
        assert!(
            Config::from_toml(EXAMPLE)
                .unwrap_err()
                .to_string()
                .contains("example")
        );
    }

    #[test]
    fn rejects_unusable_configs() {
        let wildcard = format!(
            r#"
[signal]
listen = "127.0.0.1:8443"
{KEY}
[stun]
listen = ["0.0.0.0:3478"]
"#
        );
        assert!(Config::from_toml(&wildcard).is_err());
        let half_tls = format!(
            r#"
[signal]
listen = "127.0.0.1:8443"
{KEY}
tls_cert = "a.pem"
"#
        );
        assert!(Config::from_toml(&half_tls).is_err());
        let unknown = format!(
            r#"
[signal]
listen = "127.0.0.1:8443"
{KEY}
lisen = "x"
"#
        );
        assert!(Config::from_toml(&unknown).is_err());
    }

    /// The signaling server always has a key, of some length, as typed.
    #[test]
    fn the_signaling_key_is_required() {
        let with = |keys: &str| {
            Config::from_toml(&format!(
                "[signal]\nlisten = \"127.0.0.1:8443\"\naccess_secrets = {keys}\n"
            ))
        };
        let c = with(r#"["one long enough", "and another one"]"#).unwrap();
        assert_eq!(
            c.signal.access_secrets,
            ["one long enough", "and another one"]
        );
        // None at all: refused, with the way out in the message.
        let none = Config::from_toml("[signal]\nlisten = \"127.0.0.1:8443\"\n").unwrap_err();
        assert!(none.to_string().contains("needs a key"), "{none}");
        assert!(with("[]").is_err());
        for bad in [
            r#"["short"]"#,
            r#"["  padded key here"]"#,
            r#"["line key here\n"]"#,
            r#"["ok key of length", "x"]"#,
        ] {
            assert!(with(bad).is_err(), "{bad}");
        }
        assert!(with(r#"["ten chars!!"]"#).is_ok());
        assert!(with(r#"["123456789"]"#).is_err());
    }

    #[test]
    fn the_web_password_is_optional_and_never_empty() {
        let with = |web: &str| {
            Config::from_toml(&format!(
                "[signal]\nlisten = \"127.0.0.1:8443\"\naccess_secrets = [\"a key of its own\"]\n[stun]\nlisten = [\"127.0.0.1:3478\"]\n[web]\n{web}\n"
            ))
        };
        assert!(with("").unwrap().web.unwrap().password.is_none());
        assert_eq!(
            with(r#"password = "hunter2""#)
                .unwrap()
                .web
                .unwrap()
                .password
                .as_deref(),
            Some("hunter2")
        );
        for bad in [
            r#"password = """#,
            r#"password = " x""#,
            r#"password = "x\n""#,
        ] {
            assert!(with(bad).is_err(), "{bad}");
        }
        // The page's field takes 128 characters: a longer password could never be typed.
        assert!(with(&format!("password = \"{}\"", "p".repeat(128))).is_ok());
        assert!(with(&format!("password = \"{}\"", "p".repeat(129))).is_err());
    }

    #[test]
    fn secrets_do_not_show_in_debug_output() {
        let text = format!(
            "{:?}",
            Config::from_toml(&example().replace(
                "max_allocations = 200",
                "password = \"web password\"\nmax_allocations = 200",
            ))
            .unwrap()
        );
        assert!(!text.contains("a key of its own"), "{text}");
        assert!(!text.contains("web password"), "{text}");
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
