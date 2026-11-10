//! Reachability by device ID (§4.3): stay registered with the configured
//! signaling server and turn each `Incoming` into a QUIC connection that then
//! goes through exactly the same handshake, pairing and consent as one made to
//! the listening address.
//!
//! The server is not trusted with anything: an `Incoming` that does not open
//! under the viewer's signed key is dropped, and the QUIC peer must present the
//! key the rendezvous was sealed with.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use nexa_auth::{DevicePublicKey, peer_public_key};
use nexa_session::rendezvous::{
    self, HostRendezvous,
    link::{Interfaces, LinkOptions},
    open_incoming,
    seal::SignalKeys,
    signal::{SignalClient, SignalConfig},
};
use nexa_wire::signal::{ServerBody, Trickle, group_short_id};
use tokio::sync::{Semaphore, watch};

use crate::{AgentEvent, agent::Shared};

/// Rendezvous being answered at once; more `Incoming`s are dropped.
const MAX_RENDEZVOUS: usize = 4;
/// From the answer to the viewer's QUIC handshake: checks (≤5 s) plus slack.
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(20);
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// A rendezvous in progress: its session id and where its trickles go.
type Routed = (Vec<u8>, tokio::sync::mpsc::UnboundedSender<Trickle>);

/// How the agent is reached by ID.
#[derive(Clone, Debug)]
pub struct SignalSetup {
    pub server: SignalConfig,
    /// Session sockets bind here; the IP part limits which interfaces are used.
    pub bind_ip: std::net::IpAddr,
    /// `None`: this machine's interfaces.
    pub interfaces: Option<Vec<std::net::IpAddr>>,
    /// §9.4: only devices already paired may make this host reveal its addresses.
    pub paired_only: bool,
    /// §9.4: never offer a direct address; everything goes through the relay.
    pub relay_only: bool,
    /// Port mapping on this machine's gateway, network-change watching and the
    /// Tailscale LocalAPI (§4.2, §4.5, §4.7). The binary turns it on; tests
    /// must not, since a mapping changes the real router.
    pub system_services: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SignalState {
    /// No server configured.
    #[default]
    Off,
    Connecting,
    Online,
    /// The last attempt failed; retrying.
    Error(String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SignalStatus {
    pub state: SignalState,
    /// While online: this device's ID, nine digits.
    pub short_id: Option<String>,
    /// Why NAT detection is degraded with this server, if it is.
    pub note: Option<String>,
}

impl SignalStatus {
    pub fn grouped_id(&self) -> Option<String> {
        self.short_id.as_deref().map(group_short_id)
    }
}

pub(crate) fn link_options(setup: &SignalSetup) -> LinkOptions {
    let base = if setup.system_services {
        LinkOptions::system()
    } else {
        LinkOptions::default()
    };
    LinkOptions {
        bind: std::net::SocketAddr::new(setup.bind_ip, 0),
        interfaces: match &setup.interfaces {
            Some(ips) => Interfaces::Fixed(ips.clone()),
            None => Interfaces::System,
        },
        relay_only: setup.relay_only,
        // §4.6: the relay over WebSocket goes to the server we are registered with.
        wss_relay: Some(setup.server.clone()),
        ..base
    }
}

fn report(shared: &Shared, status: SignalStatus) {
    let changed = {
        let mut s = shared
            .signal_status
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let changed = *s != status;
        *s = status.clone();
        changed
    };
    if changed {
        shared.ui.event(AgentEvent::Signal(status));
    }
}

/// Runs until shutdown: register, serve `Incoming`s, reconnect with backoff.
pub(crate) async fn run(shared: Arc<Shared>, setup: SignalSetup, keys: Arc<SignalKeys>) {
    let mut shutdown = shared.shutdown.subscribe();
    let limit = Arc::new(Semaphore::new(MAX_RENDEZVOUS));
    let mut backoff = BACKOFF_MIN;
    loop {
        if *shutdown.borrow() {
            break;
        }
        report(
            &shared,
            SignalStatus {
                state: SignalState::Connecting,
                ..Default::default()
            },
        );
        let connected = tokio::select! {
            c = SignalClient::connect(&setup.server, &keys) => c,
            _ = shutdown.wait_for(|s| *s) => break,
        };
        let mut client = match connected {
            Ok(c) => c,
            Err(e) => {
                report(
                    &shared,
                    SignalStatus {
                        state: SignalState::Error(e.to_string()),
                        ..Default::default()
                    },
                );
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = shutdown.wait_for(|s| *s) => break,
                }
                backoff = (backoff * 2).min(BACKOFF_MAX);
                continue;
            }
        };
        backoff = BACKOFF_MIN;
        let reg = client.registration.clone();
        report(
            &shared,
            SignalStatus {
                state: SignalState::Online,
                short_id: Some(reg.short_id.clone()),
                note: reg.stun_note.clone(),
            },
        );
        let sessions: Arc<Mutex<Vec<Routed>>> = Arc::default();
        loop {
            let msg = tokio::select! {
                m = client.recv() => m,
                _ = shutdown.wait_for(|s| *s) => {
                    report(&shared, SignalStatus::default());
                    return;
                }
            };
            match msg {
                Some(ServerBody::Incoming(incoming)) => {
                    let Ok(permit) = limit.clone().try_acquire_owned() else {
                        continue;
                    };
                    // Dropped silently when it does not open: the server or someone
                    // impersonating a viewer gets no reaction to probe with.
                    let Ok(offered) = open_incoming(&keys, &incoming) else {
                        continue;
                    };
                    let viewer = offered.viewer.identity;
                    if setup.paired_only
                        && shared
                            .host
                            .store
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .pinned(&viewer)
                            .is_none()
                    {
                        continue;
                    }
                    let (shared2, keys2, out, setup2, sessions2) = (
                        shared.clone(),
                        keys.clone(),
                        client.sender(),
                        setup.clone(),
                        sessions.clone(),
                    );
                    let stun = reg.stun_servers.clone();
                    tokio::spawn(async move {
                        let server = shared2.server_config.clone();
                        let session = offered.session.to_vec();
                        let r = rendezvous::answer(
                            out,
                            keys2,
                            offered,
                            &stun,
                            link_options(&setup2),
                            server,
                        )
                        .await;
                        let r = match r {
                            Ok(r) => r,
                            Err(e) => {
                                shared2.ui.event(AgentEvent::Warning(format!(
                                    "could not answer a connection request: {e}"
                                )));
                                return;
                            }
                        };
                        sessions2
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push((session.clone(), r.trickles.clone()));
                        accept(shared2, r, viewer, permit).await;
                        sessions2
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .retain(|(s, _)| *s != session);
                    });
                }
                Some(ServerBody::Trickle(t)) => {
                    let list = sessions.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some((_, tx)) = list.iter().find(|(s, _)| *s == t.session_id) {
                        let _ = tx.send(t);
                    }
                }
                Some(ServerBody::Error(e)) if e.session_id.is_empty() && e.req_id == 0 => {
                    // Replaced by another connection with our key, or the server is
                    // closing: reconnect.
                    break;
                }
                Some(_) => {}
                None => break,
            }
        }
        report(
            &shared,
            SignalStatus {
                state: SignalState::Error("lost the connection to the server".into()),
                ..Default::default()
            },
        );
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = shutdown.wait_for(|s| *s) => break,
        }
    }
    report(&shared, SignalStatus::default());
}

async fn accept(
    shared: Arc<Shared>,
    r: HostRendezvous,
    viewer: DevicePublicKey,
    permit: tokio::sync::OwnedSemaphorePermit,
) {
    let mut shutdown: watch::Receiver<bool> = shared.shutdown.subscribe();
    let incoming = tokio::select! {
        i = tokio::time::timeout(ACCEPT_TIMEOUT, r.link.endpoint.accept()) => i.ok().flatten(),
        _ = shutdown.wait_for(|s| *s) => None,
    };
    // One connection per rendezvous: nothing else may use this endpoint.
    r.link.endpoint.set_server_config(None);
    let Some(incoming) = incoming else { return };
    let remote = r
        .link
        .state()
        .active
        .map_or(incoming.remote_address(), |a| a.target.wire_addr());
    let conn = match incoming.await {
        Ok(c) => c,
        Err(e) => {
            shared.ui.event(AgentEvent::ConnectionFailed {
                remote,
                reason: e.to_string(),
            });
            return;
        }
    };
    // The QUIC peer must be the device that sealed the offer.
    if peer_public_key(&conn).ok() != Some(viewer) {
        conn.close(1u32.into(), b"identity does not match the request");
        shared.ui.event(AgentEvent::ConnectionFailed {
            remote,
            reason: "the connecting device is not the one that asked".into(),
        });
        return;
    }
    drop(permit);
    crate::agent::serve_connection(conn, shared, remote, Some(r.link.clone())).await;
    r.pump.abort();
}
