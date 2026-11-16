//! The gateway's port over the network: HTTPS serves the page, plain HTTP redirects.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use nexa_auth::OneTimeCode;
use nexa_media::{CaptureConfig, Capturer, Encoder, EncoderConfig, MediaError};
use nexa_session::input::InputSink;
use nexa_web::{
    BrowserAnswer, BrowserRequest, WebConfig, WebEvent, WebGateway, WebHost, WebScreen, WebTls,
};
use rustls::{
    DigitallySignedStruct, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

struct NoHost;

impl WebHost for NoHost {
    fn screen(&self) -> Result<WebScreen, String> {
        Err("none".into())
    }
    fn capturer(&self, _: CaptureConfig) -> nexa_media::Result<Box<dyn Capturer>> {
        Err(MediaError::Unsupported("test"))
    }
    fn encoder(&self, _: EncoderConfig) -> nexa_media::Result<Box<dyn Encoder>> {
        Err(MediaError::Unsupported("test"))
    }
    fn input_available(&self) -> bool {
        false
    }
    fn input_sink(&self) -> Result<Box<dyn InputSink>, String> {
        Err("none".into())
    }
    fn ask(&self, _: &BrowserRequest, _: Duration) -> BrowserAnswer {
        BrowserAnswer::DENY
    }
    fn event(&self, _: WebEvent) {}
}

/// Accepts exactly one certificate, by its SHA-256: what a person does when they check
/// the fingerprint the host shows.
#[derive(Debug)]
struct Pinned([u8; 32]);

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if <[u8; 32]>::from(Sha256::digest(end.as_ref())) == self.0 {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General("wrong certificate".into()))
        }
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

async fn gateway() -> (WebGateway, SocketAddr, [u8; 32]) {
    let dir = std::env::temp_dir().join(format!("nexa-web-https-{}", std::process::id()));
    let tls = WebTls::load_or_create(&dir, &["127.0.0.1".into()]).unwrap();
    let fp = tls.fingerprint;
    let g = WebGateway::bind(
        "127.0.0.1:0".parse().unwrap(),
        Arc::new(NoHost),
        OneTimeCode::generate(),
        WebConfig::default(),
        Some(tls),
    )
    .await
    .unwrap();
    let addr = g.local_addr();
    (g, addr, fp)
}

async fn exchange<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    s: &mut S,
    req: &str,
) -> String {
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.unwrap();
    String::from_utf8_lossy(&out).into_owned()
}

#[tokio::test]
async fn plain_http_is_sent_to_https() {
    let (_g, addr, _) = gateway().await;
    let mut s = TcpStream::connect(addr).await.unwrap();
    let r = exchange(
        &mut s,
        &format!("GET /app.js HTTP/1.1\r\nHost: {addr}\r\n\r\n"),
    )
    .await;
    assert!(r.starts_with("HTTP/1.1 308"), "{r}");
    assert!(
        r.contains(&format!("Location: https://{addr}/app.js\r\n")),
        "{r}"
    );

    // Only a plain name or address is echoed into the Location.
    let mut s = TcpStream::connect(addr).await.unwrap();
    let r = exchange(&mut s, "GET / HTTP/1.1\r\nHost: a/b\r\n\r\n").await;
    assert!(r.starts_with("HTTP/1.1 400"), "{r}");
}

#[tokio::test]
async fn https_serves_the_page_with_the_shown_certificate() {
    let (_g, addr, fp) = gateway().await;
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(Pinned(fp)))
    .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let tcp = TcpStream::connect(addr).await.unwrap();
    let mut tls = connector
        .connect(ServerName::try_from("127.0.0.1").unwrap(), tcp)
        .await
        .unwrap();
    let r = exchange(&mut tls, &format!("GET / HTTP/1.1\r\nHost: {addr}\r\n\r\n")).await;
    assert!(r.starts_with("HTTP/1.1 200"), "{}", &r[..r.len().min(200)]);
    assert!(r.contains("Strict-Transport-Security"));
    assert!(r.contains("<!doctype html>"));
}
