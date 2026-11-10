//! Authentication over real QUIC, using nexa-net's production socket and transport.
//! Identities are in memory and stores are in temp directories.

mod common;

use std::{sync::Arc, time::Duration};

use common::{Accepts, Control, Failure, LIMIT, Typed, link, run_host, run_viewer};
use nexa_auth::{
    AccessDecision, AccessPolicy, DeviceIdentity, Evidence, HelloAuth, HostPassword, OneTimeCode,
    PairError, PairingStore, PasswordGuard, PeerPolicy, Perms, client_config, decide, hello_auth,
    pair_exporter, peer_public_key, server_config,
};
use nexa_wire::control::{Auth, AuthStep, ClientHello};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::{
    DigitallySignedStruct, SignatureScheme,
    client::{
        ResolvesClientCert,
        danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    },
    crypto::ring,
    pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName, UnixTime},
    server::{ClientHello as TlsClientHello, ResolvesServerCert},
    sign::CertifiedKey,
    version::TLS13,
};
use zeroize::Zeroizing;

fn id() -> DeviceIdentity {
    DeviceIdentity::generate()
}

fn client(me: &DeviceIdentity, peer: PeerPolicy) -> quinn::ClientConfig {
    client_config(me, peer).unwrap()
}

fn server(me: &DeviceIdentity, peer: PeerPolicy) -> quinn::ServerConfig {
    server_config(me, peer).unwrap()
}

/// Waits for a connection that may have reported success to be closed by the peer.
async fn expect_closed(conn: quinn::Connection) {
    tokio::time::timeout(LIMIT, conn.closed())
        .await
        .expect("connection should have been closed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn correct_code_succeeds_and_yields_a_pairing() {
    let (viewer_id, host_id) = (id(), id());
    let dir = tempfile::tempdir().unwrap();
    let mut viewer_store =
        PairingStore::open(dir.path().join("viewer.json"), viewer_id.store_mac_key()).unwrap();
    let mut host_store =
        PairingStore::open(dir.path().join("host.json"), host_id.store_mac_key()).unwrap();

    let code = OneTimeCode::generate();
    let shown = code.current();

    let l = link(
        client(&viewer_id, PeerPolicy::AcceptUnknown),
        server(&host_id, PeerPolicy::AcceptUnknown),
    )
    .await;
    let (viewer_conn, host_conn) = (l.viewer.clone().unwrap(), l.host.clone().unwrap());

    // §9.2 left this as "to be verified": both ends must export the same value.
    assert_eq!(
        pair_exporter(&viewer_conn).unwrap(),
        pair_exporter(&host_conn).unwrap()
    );
    assert_eq!(peer_public_key(&viewer_conn).unwrap(), host_id.public_key());
    assert_eq!(peer_public_key(&host_conn).unwrap(), viewer_id.public_key());

    let (v, h) = tokio::join!(
        run_viewer(&viewer_conn, &viewer_id, Typed::Code(&shown)),
        run_host(&host_conn, &host_id, Accepts::Code(&code)),
    );
    let (v, h) = (v.unwrap(), h.unwrap());
    assert_eq!(v.peer(), host_id.public_key());
    assert_eq!(h.peer(), viewer_id.public_key());

    // The code is single-use.
    assert_ne!(code.current(), shown);

    // §9.3: a correct code still needs consent.
    let peer = peer_public_key(&host_conn).unwrap();
    assert!(matches!(
        decide(&peer, Evidence::Exchange(&h), &AccessPolicy::default()),
        AccessDecision::NeedsConsent(_)
    ));

    let perms = Perms {
        view: true,
        input: true,
        ..Perms::default()
    };
    host_store.add(&h, "viewer", perms, 100).unwrap();
    viewer_store.add(&v, "host", perms, 100).unwrap();
    assert!(host_store.pinned(&viewer_id.public_key()).is_some());
    assert!(viewer_store.pinned(&host_id.public_key()).is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exporters_differ_between_connections() {
    let (viewer_id, host_id) = (id(), id());
    let connect = || {
        link(
            client(&viewer_id, PeerPolicy::AcceptUnknown),
            server(&host_id, PeerPolicy::AcceptUnknown),
        )
    };
    let (a, b) = (connect().await, connect().await);
    assert_ne!(
        pair_exporter(&a.viewer.unwrap()).unwrap(),
        pair_exporter(&b.viewer.unwrap()).unwrap()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paired_devices_connect() {
    let (viewer_id, host_id) = (id(), id());
    let dir = tempfile::tempdir().unwrap();
    let mut host_store =
        PairingStore::open(dir.path().join("host.json"), host_id.store_mac_key()).unwrap();
    let (viewer_proof, host_proof) = common::pair_in_memory(&viewer_id, &host_id);
    let perms = Perms {
        view: true,
        ..Perms::default()
    };
    host_store.add(&host_proof, "viewer", perms, 1).unwrap();
    let mut viewer_store =
        PairingStore::open(dir.path().join("viewer.json"), viewer_id.store_mac_key()).unwrap();
    viewer_store.add(&viewer_proof, "host", perms, 1).unwrap();
    let pinned_host = viewer_store.records()[0].pubkey;
    assert_eq!(pinned_host, host_id.public_key());

    // The viewer pins; the host accepts the handshake and looks the key up afterwards.
    let l = link(
        client(&viewer_id, PeerPolicy::Pinned(pinned_host)),
        server(&host_id, PeerPolicy::AcceptUnknown),
    )
    .await;
    let (viewer_conn, host_conn) = (l.viewer.clone().unwrap(), l.host.clone().unwrap());

    let (sent, received) = tokio::join!(
        async {
            let mut c = Control::open(&viewer_conn).await.unwrap();
            c.send(&common::hello(Auth::Known(true))).await.unwrap();
            c
        },
        async {
            let mut c = Control::accept(&host_conn).await.unwrap();
            c.recv::<ClientHello>().await.unwrap()
        },
    );
    let _keep_stream_open = sent;
    assert_eq!(hello_auth(&received), Ok(HelloAuth::Known));

    let peer = peer_public_key(&host_conn).unwrap();
    let policy = AccessPolicy::default();
    assert!(matches!(
        decide(
            &peer,
            Evidence::Pinned(host_store.pinned(&peer).unwrap()),
            &policy
        ),
        AccessDecision::NeedsConsent(_)
    ));
    host_store.set_unattended(&peer, true).unwrap();
    assert!(matches!(
        decide(
            &peer,
            Evidence::Pinned(host_store.pinned(&peer).unwrap()),
            &policy
        ),
        AccessDecision::Allow { .. }
    ));

    // Revoking is immediate: the next decision has no evidence.
    host_store.revoke(&peer).unwrap();
    assert!(host_store.pinned(&peer).is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mutually_pinned_devices_connect() {
    let (viewer_id, host_id) = (id(), id());
    let l = link(
        client(&viewer_id, PeerPolicy::Pinned(host_id.public_key())),
        server(&host_id, PeerPolicy::Pinned(viewer_id.public_key())),
    )
    .await;
    l.viewer.unwrap();
    l.host.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_code_fails_on_both_sides() {
    let (viewer_id, host_id) = (id(), id());
    let code = OneTimeCode::generate();
    let wrong = if code.current().starts_with('A') {
        "BBBBBBBB"
    } else {
        "AAAAAAAA"
    };

    let l = link(
        client(&viewer_id, PeerPolicy::AcceptUnknown),
        server(&host_id, PeerPolicy::AcceptUnknown),
    )
    .await;
    let (viewer_conn, host_conn) = (l.viewer.clone().unwrap(), l.host.clone().unwrap());
    let (v, h) = tokio::join!(
        tokio::time::timeout(
            LIMIT,
            run_viewer(&viewer_conn, &viewer_id, Typed::Code(wrong))
        ),
        tokio::time::timeout(LIMIT, run_host(&host_conn, &host_id, Accepts::Code(&code))),
    );
    assert_eq!(
        h.unwrap().err(),
        Some(Failure::Pair(PairError::ConfirmationFailed))
    );
    // The host hangs up without confirming, so the viewer never gets to Complete.
    assert!(matches!(v.unwrap().err(), Some(Failure::Transport(_))));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn password_exchange_right_and_wrong() {
    let (viewer_id, host_id) = (id(), id());
    let stored = HostPassword::create("open sesame").unwrap();
    let guard = PasswordGuard::with_lockout(Duration::from_secs(600));

    let run = |typed: &'static str| {
        let (viewer_id, host_id, stored, guard) = (&viewer_id, &host_id, &stored, &guard);
        async move {
            let l = link(
                client(viewer_id, PeerPolicy::AcceptUnknown),
                server(host_id, PeerPolicy::AcceptUnknown),
            )
            .await;
            let (viewer_conn, host_conn) = (l.viewer.clone().unwrap(), l.host.clone().unwrap());
            let (v, h) = tokio::join!(
                tokio::time::timeout(
                    LIMIT,
                    run_viewer(&viewer_conn, viewer_id, Typed::Password(typed))
                ),
                tokio::time::timeout(
                    LIMIT,
                    run_host(&host_conn, host_id, Accepts::Password(stored, guard))
                ),
            );
            (v.unwrap(), h.unwrap())
        }
    };

    let (v, h) = run("open sesame").await;
    assert!(v.is_ok(), "{v:?}");
    let h = h.unwrap();
    assert_eq!(h.method(), nexa_auth::AuthMethod::Password);

    let (v, h) = run("open sesamE").await;
    assert_eq!(h.err(), Some(Failure::Pair(PairError::ConfirmationFailed)));
    assert!(v.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn viewer_refuses_a_host_with_the_wrong_pinned_key() {
    let (viewer_id, host_id, expected) = (id(), id(), id());
    let l = link(
        client(&viewer_id, PeerPolicy::Pinned(expected.public_key())),
        server(&host_id, PeerPolicy::AcceptUnknown),
    )
    .await;
    assert!(l.viewer.is_err(), "viewer must abort the handshake");
    assert!(
        l.host.is_err(),
        "host must not see an established connection"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_refuses_a_viewer_with_the_wrong_pinned_key() {
    let (viewer_id, host_id, expected) = (id(), id(), id());
    let l = link(
        client(&viewer_id, PeerPolicy::Pinned(host_id.public_key())),
        server(&host_id, PeerPolicy::Pinned(expected.public_key())),
    )
    .await;
    assert!(l.host.is_err());
    // In TLS 1.3 the client can finish before the server rejects its certificate.
    if let Ok(conn) = l.viewer {
        expect_closed(conn).await;
    }
}

/// Runs a relay that terminates TLS separately towards each side and forwards the
/// `ClientHello` and `AuthStep` messages. Returns what each honest end concluded.
async fn relay_attack(
    viewer_id: &DeviceIdentity,
    host_id: &DeviceIdentity,
    relay_as_host: &DeviceIdentity,
    relay_as_viewer: &DeviceIdentity,
) -> (
    Result<(), Failure>,
    Result<(), Failure>,
    OneTimeCode,
    Zeroizing<String>,
) {
    let code = OneTimeCode::generate();
    let shown = code.current();

    let toward_viewer = link(
        client(viewer_id, PeerPolicy::AcceptUnknown),
        server(relay_as_host, PeerPolicy::AcceptUnknown),
    )
    .await;
    let toward_host = link(
        client(relay_as_viewer, PeerPolicy::AcceptUnknown),
        server(host_id, PeerPolicy::AcceptUnknown),
    )
    .await;
    let (viewer_conn, relay_a) = (
        toward_viewer.viewer.clone().unwrap(),
        toward_viewer.host.clone().unwrap(),
    );
    let (relay_b, host_conn) = (
        toward_host.viewer.clone().unwrap(),
        toward_host.host.clone().unwrap(),
    );
    assert_ne!(
        pair_exporter(&viewer_conn).unwrap(),
        pair_exporter(&host_conn).unwrap(),
        "two separate TLS sessions must have different exporters"
    );

    let relay = async {
        let mut a = Control::accept(&relay_a).await.unwrap();
        let mut b = Control::open(&relay_b).await.unwrap();
        let hello: ClientHello = a.recv().await.unwrap();
        b.send(&hello).await.unwrap();
        let offer: AuthStep = b.recv().await.unwrap();
        a.send(&offer).await.unwrap();
        let answer: AuthStep = a.recv().await.unwrap();
        b.send(&answer).await.unwrap();
        // The honest host will not confirm. Forge something for the viewer so its
        // confirmation check runs rather than the connection just dropping.
        let forged = AuthStep {
            spake_msg: vec![],
            confirm: vec![0x5A; 32],
            salt: vec![],
        };
        a.send(&forged).await.unwrap();
        // Hold the connections open until the honest ends have concluded.
        tokio::time::sleep(Duration::from_secs(1)).await;
    };

    let (v, h, ()) = tokio::join!(
        tokio::time::timeout(
            LIMIT,
            run_viewer(&viewer_conn, viewer_id, Typed::Code(&shown))
        ),
        tokio::time::timeout(LIMIT, run_host(&host_conn, host_id, Accepts::Code(&code))),
        relay,
    );
    (v.unwrap().map(|_| ()), h.unwrap().map(|_| ()), code, shown)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn man_in_the_middle_fails_even_with_the_right_code() {
    let (viewer_id, host_id, relay_id) = (id(), id(), id());
    let (v, h, code, shown) = relay_attack(&viewer_id, &host_id, &relay_id, &relay_id).await;
    assert_eq!(h, Err(Failure::Pair(PairError::ConfirmationFailed)));
    assert_eq!(v, Err(Failure::Pair(PairError::ConfirmationFailed)));
    // One failed attempt is charged but the code is not consumed.
    assert_eq!(code.current(), shown);
}

/// The relay has somehow got both device keys, so the SPAKE2 identities match on both
/// legs and the only thing left to differ is the TLS exporter. Confirmation must still fail.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exporter_binding_alone_stops_a_relay() {
    let (viewer_id, host_id) = (id(), id());
    let (v, h, ..) = relay_attack(&viewer_id, &host_id, &host_id, &viewer_id).await;
    assert_eq!(h, Err(Failure::Pair(PairError::ConfirmationFailed)));
    assert_eq!(v, Err(Failure::Pair(PairError::ConfirmationFailed)));
}

// A hand-rolled attacker: test code only, and exactly what must not ship.
#[derive(Debug)]
struct TrustEverything;

impl ServerCertVerifier for TrustEverything {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
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
        vec![
            SignatureScheme::ED25519,
            SignatureScheme::ECDSA_NISTP256_SHA256,
        ]
    }
}

/// Presents whatever certificate it is told to, signing with whatever key it holds.
#[derive(Debug)]
struct Impersonator(Arc<CertifiedKey>);

impl ResolvesClientCert for Impersonator {
    fn resolve(&self, _: &[&[u8]], _: &[SignatureScheme]) -> Option<Arc<CertifiedKey>> {
        Some(self.0.clone())
    }
    fn has_certs(&self) -> bool {
        true
    }
}

impl ResolvesServerCert for Impersonator {
    fn resolve(&self, _: TlsClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.0.clone())
    }
}

/// A rustls client config that performs no validation and may send a chosen certificate.
fn attacker_client(resolver: Option<Arc<Impersonator>>) -> quinn::ClientConfig {
    let builder = rustls::ClientConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_protocol_versions(&[&TLS13])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(TrustEverything));
    let mut tls = match resolver {
        Some(r) => builder.with_client_cert_resolver(r),
        None => builder.with_no_client_auth(),
    };
    tls.alpn_protocols = vec![nexa_auth::ALPN.to_vec()];
    let mut config = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls).unwrap()));
    config.transport_config(nexa_net::cc::transport_config());
    config
}

fn attacker_server(resolver: Arc<Impersonator>) -> quinn::ServerConfig {
    let mut tls = rustls::ServerConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_protocol_versions(&[&TLS13])
        .unwrap()
        .with_no_client_auth()
        .with_cert_resolver(resolver);
    tls.alpn_protocols = vec![nexa_auth::ALPN.to_vec()];
    let mut config =
        quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls).unwrap()));
    config.transport_config(nexa_net::cc::transport_config());
    config
}

/// The victim's real certificate, as anyone who has connected to it could obtain it.
async fn certificate_of(victim: &DeviceIdentity) -> CertificateDer<'static> {
    let observer = id();
    let l = link(
        client(&observer, PeerPolicy::AcceptUnknown),
        server(victim, PeerPolicy::AcceptUnknown),
    )
    .await;
    let chain = l
        .viewer
        .unwrap()
        .peer_identity()
        .unwrap()
        .downcast::<Vec<CertificateDer<'static>>>()
        .unwrap();
    chain[0].clone()
}

fn impersonator_of(cert: CertificateDer<'static>) -> Arc<Impersonator> {
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).unwrap();
    let der = PrivatePkcs8KeyDer::from(key.serialize_der());
    let signer = ring::sign::any_eddsa_type(&der).unwrap();
    Arc::new(Impersonator(Arc::new(CertifiedKey::new(
        vec![cert],
        signer,
    ))))
}

/// A stolen certificate without its private key must not pass as a host, whether the
/// viewer pins the key or not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stolen_host_certificate_without_the_key_is_refused() {
    let (viewer_id, victim) = (id(), id());
    let cert = certificate_of(&victim).await;
    for policy in [
        PeerPolicy::AcceptUnknown,
        PeerPolicy::Pinned(victim.public_key()),
    ] {
        let l = link(
            client(&viewer_id, policy),
            attacker_server(impersonator_of(cert.clone())),
        )
        .await;
        assert!(l.viewer.is_err(), "viewer accepted a forged handshake");
    }
}

/// The same from the other side: the viewer's certificate is not enough to be the viewer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stolen_viewer_certificate_without_the_key_is_refused() {
    let (host_id, victim) = (id(), id());
    let cert = certificate_of(&victim).await;
    let l = link(
        attacker_client(Some(impersonator_of(cert))),
        server(&host_id, PeerPolicy::AcceptUnknown),
    )
    .await;
    assert!(l.host.is_err(), "host accepted a forged handshake");
    if let Ok(conn) = l.viewer {
        expect_closed(conn).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_requires_a_client_certificate() {
    let host_id = id();
    let l = link(
        attacker_client(None),
        server(&host_id, PeerPolicy::AcceptUnknown),
    )
    .await;
    assert!(
        l.host.is_err(),
        "host accepted a client with no certificate"
    );
    if let Ok(conn) = l.viewer {
        expect_closed(conn).await;
    }
}

/// A stranger who finishes TLS but offers no proof gets nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_peer_gets_past_tls_but_is_denied() {
    let (stranger, host_id) = (id(), id());
    let dir = tempfile::tempdir().unwrap();
    let store = PairingStore::open(dir.path().join("host.json"), host_id.store_mac_key()).unwrap();
    let l = link(
        client(&stranger, PeerPolicy::AcceptUnknown),
        server(&host_id, PeerPolicy::AcceptUnknown),
    )
    .await;
    let host_conn = l.host.clone().unwrap();
    let peer = peer_public_key(&host_conn).unwrap();
    assert!(store.pinned(&peer).is_none());
    assert!(matches!(
        decide(&peer, Evidence::None, &AccessPolicy::default()),
        AccessDecision::Deny(_)
    ));
}
