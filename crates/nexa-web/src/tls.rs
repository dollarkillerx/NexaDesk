//! HTTPS for the page, on the same port as plain HTTP.
//!
//! Browsers on a LAN reach the host by an IP address, which no public CA signs, so by
//! default the host makes its own certificate. The browser warns about it once; after
//! that the page, its script and the WebSocket cannot be read or changed on the way,
//! which plain HTTP on a LAN cannot promise. A certificate the person provides
//! (`web-cert.pem` and `web-key.pem` in the data directory, e.g. from `tailscale cert`)
//! is used instead and needs no warning at all.
//!
//! The self-signed certificate is kept and reused while the names it covers stay the
//! same, so a browser that accepted it once is not asked again after a restart.

use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
};

use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, SanType};
use rustls::{
    ServerConfig,
    pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
};
use sha2::{Digest, Sha256};
use tokio_rustls::TlsAcceptor;

/// The person's own certificate chain and key, if present.
pub const CUSTOM_CERT: &str = "web-cert.pem";
pub const CUSTOM_KEY: &str = "web-key.pem";
/// The host's own certificate, its key, and the names it was made for.
const SELF_CERT: &str = "web-selfsigned-cert.pem";
const SELF_KEY: &str = "web-selfsigned-key.pem";
const SELF_NAMES: &str = "web-selfsigned-names.txt";
/// Kept well under a year: some browsers refuse longer-lived certificates outright.
const VALID_DAYS: i64 = 390;
/// Made again this long before it expires.
const RENEW_DAYS: i64 = 30;

#[derive(Clone)]
pub struct WebTls {
    pub(crate) acceptor: TlsAcceptor,
    /// SHA-256 of the leaf certificate, as browsers show it.
    pub fingerprint: [u8; 32],
    /// Whether this is the person's own certificate rather than one we made.
    pub custom: bool,
}

impl std::fmt::Debug for WebTls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebTls")
            .field("fingerprint", &self.fingerprint_text())
            .field("custom", &self.custom)
            .finish()
    }
}

impl WebTls {
    /// The person's certificate from `dir`, else ours, made or renewed for `names`
    /// (host names and IP addresses the page may be opened by).
    pub fn load_or_create(dir: &Path, names: &[String]) -> io::Result<Self> {
        let (cert, key) = (dir.join(CUSTOM_CERT), dir.join(CUSTOM_KEY));
        if cert.exists() || key.exists() {
            let chain = read_chain(&cert)?;
            let key = read_key(&key)?;
            return Self::from_parts(chain, key, true);
        }
        let (chain, key) = self_signed(dir, names)?;
        Self::from_parts(chain, key, false)
    }

    fn from_parts(
        chain: Vec<CertificateDer<'static>>,
        key: PrivateKeyDer<'static>,
        custom: bool,
    ) -> io::Result<Self> {
        let leaf = chain.first().ok_or_else(|| invalid("no certificate"))?;
        let fingerprint = Sha256::digest(leaf.as_ref()).into();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| invalid(&e.to_string()))?
            .with_no_client_auth()
            .with_single_cert(chain, key)
            .map_err(|e| invalid(&format!("certificate and key: {e}")))?;
        Ok(Self {
            acceptor: TlsAcceptor::from(Arc::new(config)),
            fingerprint,
            custom,
        })
    }

    /// `AB:CD:…`, as a browser's certificate viewer shows it.
    pub fn fingerprint_text(&self) -> String {
        self.fingerprint
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(":")
    }
}

/// What a browser may type to reach this machine: its interface addresses, `localhost`,
/// and its Tailscale name. Link-local IPv6 is left out: it cannot be written in a URL
/// without a zone, nor in a certificate with one.
pub fn certificate_names() -> Vec<String> {
    let mut names: Vec<String> = nexa_net::discovery::local_ips()
        .into_iter()
        .filter(|ip| match ip {
            std::net::IpAddr::V4(v4) => !v4.is_unspecified() && !v4.is_link_local(),
            std::net::IpAddr::V6(v6) => {
                !v6.is_unspecified() && !v6.is_unicast_link_local() && !v6.is_loopback()
            }
        })
        .map(|ip| ip.to_string())
        .collect();
    names.push("localhost".into());
    if let Some(ts) = nexa_net::tailscale::query_localapi()
        && !ts.self_dns_name.is_empty()
    {
        names.push(ts.self_dns_name);
    }
    names
}

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_string())
}

fn read_chain(path: &Path) -> io::Result<Vec<CertificateDer<'static>>> {
    let chain = CertificateDer::pem_file_iter(path)
        .map_err(|e| invalid(&format!("{}: {e}", path.display())))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| invalid(&format!("{}: {e}", path.display())))?;
    if chain.is_empty() {
        return Err(invalid(&format!("{}: no certificate", path.display())));
    }
    Ok(chain)
}

fn read_key(path: &Path) -> io::Result<PrivateKeyDer<'static>> {
    PrivateKeyDer::from_pem_file(path).map_err(|e| invalid(&format!("{}: {e}", path.display())))
}

/// The names, sorted and deduplicated, one per line: what the stored certificate covers.
fn names_text(names: &[String]) -> String {
    let mut v: Vec<&str> = names
        .iter()
        .map(|n| n.trim())
        .filter(|n| !n.is_empty())
        .collect();
    v.sort_unstable();
    v.dedup();
    v.join("\n")
}

fn self_signed(
    dir: &Path,
    names: &[String],
) -> io::Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    fs::create_dir_all(dir)?;
    let paths = |n: &str| -> PathBuf { dir.join(n) };
    let wanted = names_text(names);
    let fresh = fs::read_to_string(paths(SELF_NAMES)).is_ok_and(|n| n == wanted)
        && read_chain(&paths(SELF_CERT)).is_ok_and(|c| !expiring(&c[0]));
    if fresh
        && let (Ok(chain), Ok(key)) = (read_chain(&paths(SELF_CERT)), read_key(&paths(SELF_KEY)))
    {
        return Ok((chain, key));
    }

    // The key outlives certificates: a browser that pinned nothing still sees the same key.
    let key = match fs::read_to_string(paths(SELF_KEY)) {
        Ok(pem) => KeyPair::from_pem(&pem).map_err(|e| invalid(&e.to_string()))?,
        Err(_) => {
            let key = KeyPair::generate().map_err(|e| invalid(&e.to_string()))?;
            write_private(&paths(SELF_KEY), key.serialize_pem().as_bytes())?;
            key
        }
    };
    let mut params = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "NexaDesk browser access");
    dn.push(DnType::OrganizationName, "NexaDesk (self-signed)");
    params.distinguished_name = dn;
    params.subject_alt_names = wanted
        .lines()
        .filter_map(|n| match n.parse::<std::net::IpAddr>() {
            Ok(ip) => Some(SanType::IpAddress(ip)),
            Err(_) => n.to_string().try_into().ok().map(SanType::DnsName),
        })
        .collect();
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now - time::Duration::days(1);
    params.not_after = now + time::Duration::days(VALID_DAYS);
    let cert = params
        .self_signed(&key)
        .map_err(|e| invalid(&e.to_string()))?;
    fs::write(paths(SELF_CERT), cert.pem())?;
    fs::write(paths(SELF_NAMES), &wanted)?;
    let der = PrivateKeyDer::try_from(key.serialize_der()).map_err(invalid)?;
    Ok((vec![cert.der().clone()], der))
}

/// Whether a certificate ends within [`RENEW_DAYS`], or cannot be read.
fn expiring(cert: &CertificateDer<'_>) -> bool {
    let Ok((_, parsed)) = x509_parser::parse_x509_certificate(cert.as_ref()) else {
        return true;
    };
    let end = parsed.validity().not_after.timestamp();
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    end - now < RENEW_DAYS * 86_400
}

fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::{io::Write, os::unix::fs::OpenOptionsExt};
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(bytes)
    }
    #[cfg(not(unix))]
    fs::write(path, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "nexa-web-tls-{}-{}",
            std::process::id(),
            rand_suffix()
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// Distinct for every call: tests run in parallel and each removes its own folder.
    fn rand_suffix() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        NEXT.fetch_add(1, Relaxed)
    }

    #[test]
    fn the_same_names_keep_the_same_certificate() {
        let dir = temp();
        let names = vec!["192.168.1.20".to_string(), "studio.local".to_string()];
        let a = WebTls::load_or_create(&dir, &names).unwrap();
        let b =
            WebTls::load_or_create(&dir, &["studio.local".into(), "192.168.1.20".into()]).unwrap();
        assert_eq!(a.fingerprint, b.fingerprint);
        assert!(!a.custom);
        let c = WebTls::load_or_create(&dir, &["10.0.0.5".into()]).unwrap();
        assert_ne!(a.fingerprint, c.fingerprint, "new names, new certificate");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_provided_certificate_wins() {
        let dir = temp();
        let key = KeyPair::generate().unwrap();
        let cert = CertificateParams::new(vec!["mac.tail1234.ts.net".into()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        fs::write(dir.join(CUSTOM_CERT), cert.pem()).unwrap();
        fs::write(dir.join(CUSTOM_KEY), key.serialize_pem()).unwrap();
        let tls = WebTls::load_or_create(&dir, &["192.168.1.20".into()]).unwrap();
        assert!(tls.custom);
        assert_eq!(
            tls.fingerprint,
            <[u8; 32]>::from(Sha256::digest(cert.der()))
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_broken_provided_certificate_is_an_error() {
        let dir = temp();
        fs::write(dir.join(CUSTOM_CERT), "not a certificate").unwrap();
        assert!(WebTls::load_or_create(&dir, &[]).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
