//! Signaling and pairing for one browser over its WebSocket, then the session itself.
//!
//! Messages are JSON objects with a `type`; byte fields are base64.
//!
//! ```text
//! browser → host  {type:"offer", sdp, browser}
//! host → browser  {type:"answer", sdp, salt, msg}         SPAKE2 offer
//! browser → host  {type:"auth", msg, confirm}
//! host → browser  {type:"waiting"}                        the person is being asked
//! host → browser  {type:"accepted", confirm, input}       or {type:"denied"}
//! host → browser  {type:"ended", reason}
//! host → browser  {type:"error", code}                    then the socket closes
//! ```
//!
//! The WebRTC connection starts with the answer, so ICE and DTLS run while the person
//! decides, but nothing is sent or accepted on it until `accepted`.

use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering::Relaxed},
    },
    time::Duration,
};

use data_encoding::BASE64;
use futures_util::{SinkExt, StreamExt};
use nexa_auth::{
    PairError, Source,
    browser::{BrowserPairing, DtlsFingerprint},
};
use nexa_wire::control::AuthStep;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{net::TcpStream, sync::mpsc, time::timeout};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

use crate::{
    BrowserRequest, Shared, WebEvent, clean_label,
    input::InputThread,
    media::MediaThread,
    rtc::{self, Cmd, Ended},
};

const STEP_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_MESSAGE: usize = 64 * 1024;

type Ws = WebSocketStream<TcpStream>;

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum FromBrowser {
    Offer {
        sdp: String,
        #[serde(default)]
        browser: String,
    },
    Auth {
        msg: String,
        confirm: String,
    },
    Bye,
}

async fn send(ws: &mut Ws, v: Value) -> bool {
    ws.send(Message::text(v.to_string())).await.is_ok()
}

async fn fail(ws: &mut Ws, code: &str) {
    let _ = send(ws, json!({"type": "error", "code": code})).await;
    let _ = ws.close(None).await;
}

/// The next message, `None` when the socket closed, errored or timed out.
async fn recv(ws: &mut Ws, within: Duration) -> Option<FromBrowser> {
    loop {
        let msg = timeout(within, ws.next()).await.ok()??.ok()?;
        match msg {
            Message::Text(t) if t.len() <= MAX_MESSAGE => return serde_json::from_str(&t).ok(),
            Message::Ping(_) | Message::Pong(_) => continue,
            _ => return None,
        }
    }
}

fn pair_error_code(e: &PairError) -> &'static str {
    match e {
        PairError::TooManyAttempts | PairError::LockedOut => "attempts",
        PairError::SecretConsumed => "expired",
        _ => "code",
    }
}

/// Clears the busy flag however the session ends.
struct Busy<'a>(&'a AtomicBool);

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.store(false, Relaxed);
    }
}

pub(crate) async fn run(mut ws: Ws, peer: SocketAddr, local: SocketAddr, shared: Arc<Shared>) {
    let peer = SocketAddr::new(peer.ip().to_canonical(), peer.port());
    let host = shared.host.clone();
    let refused = |reason: &str| {
        host.event(WebEvent::Refused {
            remote: peer,
            reason: reason.to_string(),
        })
    };
    let source = Source::of(peer.ip());
    if !shared.limiter.allow(source) {
        refused("too many attempts from this address");
        return fail(&mut ws, "rate").await;
    }

    let Some(FromBrowser::Offer { sdp, browser }) = recv(&mut ws, STEP_TIMEOUT).await else {
        return fail(&mut ws, "protocol").await;
    };
    let browser = clean_label(&browser);
    if shared.busy.swap(true, Relaxed) {
        refused("another browser is connected");
        return fail(&mut ws, "busy").await;
    }
    let _busy = Busy(&shared.busy);
    if host.busy() {
        refused("a viewer is connected");
        return fail(&mut ws, "busy").await;
    }

    let Some(browser_fp) = DtlsFingerprint::from_sdp(&sdp) else {
        return fail(&mut ws, "protocol").await;
    };
    let prepared = match rtc::prepare(&sdp, local.ip(), &shared.config).await {
        Ok(p) => p,
        Err(_) => return fail(&mut ws, "webrtc").await,
    };
    let host_fp = DtlsFingerprint(prepared.host_fingerprint);
    let (mut pairing, offer) =
        match BrowserPairing::with_code(browser_fp, host_fp, &shared.code, source) {
            Ok(p) => p,
            Err(e) => {
                refused(&e.to_string());
                return fail(&mut ws, pair_error_code(&e)).await;
            }
        };
    let answer = json!({
        "type": "answer",
        "sdp": prepared.answer.to_sdp_string(),
        "salt": BASE64.encode(&offer.salt),
        "msg": BASE64.encode(&offer.spake_msg),
    });
    if !send(&mut ws, answer).await {
        return;
    }

    // ICE and DTLS go ahead now; nothing is sent until Cmd::Start.
    let (cmd_tx, cmd_rx) = mpsc::channel(4);
    let mut driver = tokio::spawn(rtc::drive(
        prepared.rtc,
        prepared.socket,
        cmd_rx,
        shared.config.clone(),
    ));

    let Some(FromBrowser::Auth { msg, confirm }) = recv(&mut ws, STEP_TIMEOUT).await else {
        driver.abort();
        return fail(&mut ws, "protocol").await;
    };
    let (Ok(msg), Ok(confirm)) = (
        BASE64.decode(msg.as_bytes()),
        BASE64.decode(confirm.as_bytes()),
    ) else {
        driver.abort();
        return fail(&mut ws, "protocol").await;
    };
    let (host_step, verified) = match pairing.on_step(&AuthStep {
        spake_msg: msg,
        confirm,
        salt: Vec::new(),
    }) {
        Ok(v) => v,
        Err(e) => {
            driver.abort();
            refused(&e.to_string());
            return fail(&mut ws, pair_error_code(&e)).await;
        }
    };

    // Ask the person at the host, and stop asking if the browser leaves.
    let _ = send(&mut ws, json!({"type": "waiting"})).await;
    let id = shared.next_id.fetch_add(1, Relaxed);
    let cancelled = Arc::new(AtomicBool::new(false));
    let request = BrowserRequest {
        id,
        browser: browser.clone(),
        remote: peer,
        fingerprint: verified.fingerprint(),
        input_available: host.input_available(),
        cancelled: cancelled.clone(),
    };
    let ask = {
        let (host, consent_timeout) = (host.clone(), shared.config.consent_timeout);
        tokio::task::spawn_blocking(move || host.ask(&request, consent_timeout))
    };
    tokio::pin!(ask);
    let answer = loop {
        tokio::select! {
            a = &mut ask => break a.unwrap_or(crate::BrowserAnswer::DENY),
            _ = shared.kick.notified() => {
                cancelled.store(true, Relaxed);
                driver.abort();
                let _ = (&mut ask).await;
                return fail(&mut ws, "denied").await;
            }
            m = ws.next() => match m {
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                _ => {
                    cancelled.store(true, Relaxed);
                    driver.abort();
                    let _ = (&mut ask).await;
                    refused("the browser left before an answer");
                    return;
                }
            },
        }
    };
    if !answer.allow {
        driver.abort();
        refused("denied");
        let _ = send(&mut ws, json!({"type": "denied"})).await;
        let _ = ws.close(None).await;
        return;
    }

    let screen = match host.screen() {
        Ok(s) => s,
        Err(e) => {
            driver.abort();
            refused(&e);
            return fail(&mut ws, "screen").await;
        }
    };
    let input_allowed = answer.input && host.input_available();
    let accepted = json!({
        "type": "accepted",
        "confirm": BASE64.encode(&host_step.confirm),
        "input": input_allowed,
    });
    if !send(&mut ws, accepted).await {
        driver.abort();
        return;
    }

    let (frame_tx, frame_rx) = mpsc::channel(2);
    let media = MediaThread::spawn(host.clone(), screen, &shared.config, frame_tx);
    let input = input_allowed.then(|| InputThread::spawn(host.clone(), screen));
    let _ = cmd_tx
        .send(Cmd::Start {
            frames: frame_rx,
            control: media.control.clone(),
            input,
        })
        .await;
    host.event(WebEvent::SessionStarted {
        id,
        browser,
        remote: peer,
        fingerprint: verified.fingerprint(),
        input: input_allowed,
    });

    let reason = loop {
        tokio::select! {
            _ = shared.kick.notified() => break "ended by the host".to_string(),
            ended = &mut driver => break match ended {
                Ok(Ended::Disconnected) => "the browser disconnected".to_string(),
                Ok(Ended::Error(e)) => e,
                Err(_) => "stopped".to_string(),
            },
            m = ws.next() => match m {
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                Some(Ok(Message::Text(t))) if matches!(serde_json::from_str(&t), Ok(FromBrowser::Bye)) => {
                    break "the browser disconnected".to_string();
                }
                Some(Ok(Message::Text(_))) => continue,
                _ => break "the browser disconnected".to_string(),
            },
        }
    };
    driver.abort();
    drop(cmd_tx);
    drop(media);
    let _ = send(&mut ws, json!({"type": "ended", "reason": reason})).await;
    let _ = ws.close(None).await;
    host.event(WebEvent::SessionEnded { id, reason });
}
