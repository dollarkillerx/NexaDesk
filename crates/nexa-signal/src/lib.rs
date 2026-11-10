//! `nexa-signal`: signaling over WebSocket, a STUN responder and a UDP relay in
//! one self-hostable process (docs/DETAILED_DESIGN.md §4.3, §4.6).
//!
//! The server is untrusted by design. It forwards blobs sealed end to end, and
//! pairing and consent happen inside the peers' QUIC connection, so a hostile
//! or compromised server can refuse service or observe metadata, but cannot
//! read sessions or get past pairing (§9.2).

pub mod config;
pub mod hub;
pub mod limit;
pub mod relay;
pub mod server;
pub mod stun;

use std::{path::Path, sync::Arc};

use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};

pub use config::Config;
pub use server::{Server, Settings, Sockets, start};

/// A TLS configuration from PEM files.
pub fn tls_from_pem(cert: &Path, key: &Path) -> Result<Arc<rustls::ServerConfig>, String> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
        .map_err(|e| format!("{}: {e}", cert.display()))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("{}: {e}", cert.display()))?;
    if certs.is_empty() {
        return Err(format!("{}: no certificate", cert.display()));
    }
    let key = PrivateKeyDer::from_pem_file(key).map_err(|e| format!("{}: {e}", key.display()))?;
    tls_from_der(certs, key)
}

pub fn tls_from_der(
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<Arc<rustls::ServerConfig>, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| e.to_string())?;
    Ok(Arc::new(cfg))
}

/// Binds everything a [`Config`] asks for.
pub async fn bind(config: &Config) -> Result<(Settings, Sockets), String> {
    let tls = match (&config.signal.tls_cert, &config.signal.tls_key) {
        (Some(c), Some(k)) => Some(tls_from_pem(c, k)?),
        _ => None,
    };
    let ws = tokio::net::TcpListener::bind(config.signal.listen)
        .await
        .map_err(|e| format!("signaling on {}: {e}", config.signal.listen))?;
    let mut stun = Vec::new();
    for a in &config.stun.listen {
        stun.push(Arc::new(
            nexa_net::udp::UdpIo::bind_tokio(*a).map_err(|e| format!("STUN on {a}: {e}"))?,
        ));
    }
    let relay = match &config.relay {
        Some(r) => Some(Arc::new(
            nexa_net::udp::UdpIo::bind_tokio(r.listen)
                .map_err(|e| format!("relay on {}: {e}", r.listen))?,
        )),
        None => None,
    };
    let relay_limits = config.relay.as_ref().map_or(
        relay::RelayLimits {
            bandwidth_bps: 0,
            max_sessions: 0,
        },
        |r| relay::RelayLimits {
            bandwidth_bps: r.bandwidth_bps,
            max_sessions: r.max_sessions,
        },
    );
    Ok((
        Settings {
            tls,
            server_names: config.signal.server_names.clone(),
            stun_advertise: config.stun_advertised(),
            relay_advertise: config.relay_advertised(),
            relay: relay_limits,
            relay_token_ttl_secs: config.relay.as_ref().map_or(3600, |r| r.token_ttl_secs),
            limits: config.limits.clone(),
        },
        Sockets { ws, stun, relay },
    ))
}
