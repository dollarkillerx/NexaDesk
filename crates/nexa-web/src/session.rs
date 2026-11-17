//! Signaling and pairing for one browser over its WebSocket, then the session itself.
//!
//! Messages are JSON objects with a `type`; byte fields are base64.
//!
//! ```text
//! browser → host  {type:"offer", sdp, browser, auth}   auth: "code", "password" or "request"
//! host → browser  {type:"answer", sdp, salt, msg}         SPAKE2 offer (no salt, msg for "request")
//! browser → host  {type:"auth", msg, confirm}             not for "request"
//! host → browser  {type:"waiting"}                        the person is being asked
//! host → browser  {type:"accepted", confirm, input, clipboard, terminal, os, name, cursor}   or {type:"denied"}
//! host → browser  {type:"candidates", list}       through the server, once let in: this
//!                                                 host's own addresses, to try directly
//! host → browser  {type:"ended", reason}
//! host → browser  {type:"error", code}                    then the socket closes
//! ```
//!
//! The WebRTC connection starts with the answer, so ICE and DTLS run while the person
//! decides, but nothing is sent or accepted on it until `accepted`.
//!
//! The same exchange runs over the browser's WebSocket to this host, or, for a browser
//! anywhere, frame by frame through the signaling server ([`Pipe::Relayed`]); the
//! pairing is bound to both DTLS fingerprints either way, so the server in between
//! learns nothing it could use.

use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed},
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
use tokio::{sync::mpsc, time::timeout};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

use crate::{
    BrowserAuth, BrowserRequest, Shared, WebEvent, clean_label,
    cursor::CursorFeed,
    input::{Grants, InputThread},
    media::MediaThread,
    rtc::{self, Cmd, Ended},
};

const STEP_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_MESSAGE: usize = 64 * 1024;

type Ws = WebSocketStream<nexa_web_page::http::Conn>;

/// Where the browser's text frames come and go.
pub(crate) enum Pipe {
    /// The browser's own WebSocket to this host.
    Ws(Box<Ws>),
    /// Through the signaling server: frames in, frames out.
    Relayed {
        rx: mpsc::Receiver<String>,
        tx: mpsc::Sender<String>,
    },
}

impl Pipe {
    async fn send_text(&mut self, text: String) -> bool {
        match self {
            Pipe::Ws(ws) => ws.send(Message::text(text)).await.is_ok(),
            Pipe::Relayed { tx, .. } => tx.send(text).await.is_ok(),
        }
    }

    /// The next text frame; `None` once the browser is gone. Pings are skipped.
    async fn next_text(&mut self) -> Option<String> {
        match self {
            Pipe::Ws(ws) => loop {
                match ws.next().await? {
                    Ok(Message::Text(t)) if t.len() <= MAX_MESSAGE => return Some(t.to_string()),
                    Ok(Message::Ping(_) | Message::Pong(_)) => continue,
                    _ => return None,
                }
            },
            Pipe::Relayed { rx, .. } => rx.recv().await.filter(|t| t.len() <= MAX_MESSAGE),
        }
    }

    async fn close(&mut self) {
        if let Pipe::Ws(ws) = self {
            let _ = ws.close(None).await;
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum FromBrowser {
    Offer {
        sdp: String,
        #[serde(default)]
        browser: String,
        /// "password" for the host's fixed password; anything else is the code.
        #[serde(default)]
        auth: String,
        /// "terminal" for the command line only; anything else is the screen.
        #[serde(default)]
        view: String,
    },
    Auth {
        msg: String,
        confirm: String,
    },
    Bye,
}

async fn send(ws: &mut Pipe, v: Value) -> bool {
    ws.send_text(v.to_string()).await
}

async fn fail(ws: &mut Pipe, code: &str) {
    let _ = send(ws, json!({"type": "error", "code": code})).await;
    ws.close().await;
}

/// The next message, `None` when the browser left, said nonsense or took too long.
async fn recv(ws: &mut Pipe, within: Duration) -> Option<FromBrowser> {
    let text = timeout(within, ws.next_text()).await.ok()??;
    serde_json::from_str(&text).ok()
}

fn pair_error_code(e: &PairError, password: bool) -> &'static str {
    match e {
        PairError::LockedOut if password => "locked",
        PairError::TooManyAttempts | PairError::LockedOut => "attempts",
        PairError::SecretConsumed => "expired",
        _ if password => "password",
        _ => "code",
    }
}

/// Counts this browser among the active ones until it is gone, however it ends.
struct Active<'a>(&'a AtomicUsize);

impl Drop for Active<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Relaxed);
    }
}

/// The browsers this host lets in at once, whatever the host itself allows.
const MAX_BROWSERS: usize = 8;

/// Forgets a session's kick when it ends.
struct Kick<'a>(&'a Shared, u64);

impl Drop for Kick<'_> {
    fn drop(&mut self) {
        self.0
            .kicks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.1);
    }
}

/// One browser, from its first message to the end of its session. `peer` is where it
/// is (for the person, and for counting pairing attempts); `path` how media reaches it.
pub(crate) async fn run(mut ws: Pipe, peer: SocketAddr, path: rtc::Path, shared: Arc<Shared>) {
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

    let Some(FromBrowser::Offer {
        sdp,
        browser,
        auth,
        view,
    }) = recv(&mut ws, STEP_TIMEOUT).await
    else {
        return fail(&mut ws, "protocol").await;
    };
    let method = match auth.as_str() {
        "password" => BrowserAuth::Password,
        // Only through the server, where the browser reached this host by its device ID.
        "request" if matches!(ws, Pipe::Relayed { .. }) => BrowserAuth::Request,
        "request" => {
            refused("asked without a code on a direct connection");
            return fail(&mut ws, "code").await;
        }
        _ => BrowserAuth::Code,
    };
    let by_password = method == BrowserAuth::Password;
    let password = if by_password {
        let current = shared
            .password
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        match current {
            Some(p) => Some(p),
            None => {
                refused("no fixed password is set");
                return fail(&mut ws, "nopassword").await;
            }
        }
    } else {
        None
    };
    let browser = clean_label(&browser);
    // The command line alone: asked for, or all this host has.
    let terminal_only = view == "terminal" || !host.has_screen();
    // The host counts everyone, apps and browsers; this one is not among them yet.
    if host.busy() || shared.active.load(Relaxed) >= MAX_BROWSERS {
        refused("no room for another person");
        return fail(&mut ws, "busy").await;
    }
    shared.active.fetch_add(1, Relaxed);
    let _active = Active(&shared.active);

    let Some(browser_fp) = DtlsFingerprint::from_sdp(&sdp) else {
        return fail(&mut ws, "protocol").await;
    };
    let relayed = matches!(path, rtc::Path::Turn { .. });
    let prepared = match rtc::prepare(&sdp, path, &shared.config).await {
        Ok(p) => p,
        Err(_) => return fail(&mut ws, "webrtc").await,
    };
    let host_fp = DtlsFingerprint(prepared.host_fingerprint);
    let started = match (method, &password) {
        (BrowserAuth::Request, _) => None,
        (_, Some((stored, guard))) => Some(BrowserPairing::with_password(
            browser_fp, host_fp, stored, guard, source,
        )),
        (_, None) => Some(BrowserPairing::with_code(
            browser_fp,
            host_fp,
            &shared.code,
            source,
        )),
    };
    let (pairing, offer) = match started {
        None => (None, None),
        Some(Ok((p, o))) => (Some(p), Some(o)),
        Some(Err(e)) => {
            refused(&e.to_string());
            return fail(&mut ws, pair_error_code(&e, by_password)).await;
        }
    };
    let mut answer = json!({
        "type": "answer",
        "sdp": prepared.answer.to_sdp_string(),
    });
    if let Some(offer) = &offer {
        answer["salt"] = BASE64.encode(&offer.salt).into();
        answer["msg"] = BASE64.encode(&offer.spake_msg).into();
    }
    if !send(&mut ws, answer).await {
        return;
    }

    // ICE and DTLS go ahead now; nothing is sent until Cmd::Start.
    let (cmd_tx, cmd_rx) = mpsc::channel(4);
    let mut driver = tokio::spawn(rtc::drive(
        prepared.rtc,
        prepared.net,
        prepared.lan,
        cmd_rx,
        shared.config.clone(),
    ));

    // The exchange proves the code or password and binds both fingerprints; a request
    // has neither, and the browser's certificate is taken from its offer.
    let (confirm, fingerprint) = match pairing {
        Some(mut pairing) => {
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
                    return fail(&mut ws, pair_error_code(&e, by_password)).await;
                }
            };
            (Some(host_step.confirm), verified.fingerprint())
        }
        None => (None, browser_fp),
    };

    // Ask the person at the host, and stop asking if the browser leaves.
    let _ = send(&mut ws, json!({"type": "waiting"})).await;
    let id = shared.next_id.fetch_add(1, Relaxed);
    let cancelled = Arc::new(AtomicBool::new(false));
    let request = BrowserRequest {
        id,
        browser: browser.clone(),
        remote: peer,
        fingerprint,
        input_available: host.input_available(),
        auth: method,
        terminal_only,
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
            m = ws.next_text() => match m {
                // Nothing is expected while the person decides.
                Some(_) => continue,
                None => {
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
        ws.close().await;
        return;
    }

    // A terminal-only session without the terminal would show nothing.
    if terminal_only && !answer.terminal {
        driver.abort();
        refused("the command line was not allowed");
        return fail(&mut ws, "noterminal").await;
    }
    let screen = if terminal_only {
        None
    } else {
        match host.screen() {
            Ok(s) => Some(s),
            Err(e) => {
                driver.abort();
                refused(&e);
                return fail(&mut ws, "screen").await;
            }
        }
    };
    let input_allowed = screen.is_some() && answer.input && host.input_available();
    let clipboard_allowed = screen.is_some() && answer.clipboard;
    let terminal_allowed = answer.terminal;
    // The cursor beside the picture, where the host can read it.
    let cursor_source = screen.and_then(|_| host.cursor_source());
    let accepted = json!({
        "type": "accepted",
        "confirm": confirm.map(|c| BASE64.encode(&c)).unwrap_or_default(),
        "input": input_allowed,
        "clipboard": clipboard_allowed,
        "terminal": terminal_allowed,
        "video": screen.is_some(),
        // Shortcuts are carried over to the host's conventions (Ctrl+C → ⌘C on a Mac).
        "os": std::env::consts::OS,
        "name": shared.config.name,
        "cursor": cursor_source.is_some(),
        // The screen in points: the size the cursor is drawn at.
        "points": screen.map(|s| [s.size_points.0, s.size_points.1]),
    });
    if !send(&mut ws, accepted).await {
        driver.abort();
        return;
    }

    let (frame_tx, frame_rx) = mpsc::channel(2);
    let media = screen.map(|screen| {
        MediaThread::spawn(
            host.clone(),
            id,
            screen,
            &shared.config,
            cursor_source.is_none(),
            frame_tx,
        )
    });
    // Clipboard text and the cursor, to the page.
    let (out_tx, out_rx) = mpsc::channel(32);
    let cursor = match (screen, cursor_source, &media) {
        (Some(screen), Some(source), Some(m)) => Some(CursorFeed::spawn(
            source,
            screen,
            m.control.cursor_resync.clone(),
            out_tx.clone(),
        )),
        _ => None,
    };
    let input = screen
        .filter(|_| input_allowed || clipboard_allowed)
        .map(|screen| {
            InputThread::spawn(
                host.clone(),
                screen,
                Grants {
                    input: input_allowed,
                    clipboard: clipboard_allowed,
                },
                out_tx,
            )
        });
    let _ = cmd_tx
        .send(Cmd::Start {
            frames: media.as_ref().map(|_| frame_rx),
            control: media.as_ref().map(|m| m.control.clone()),
            input,
            outbound: out_rx,
            terminal: terminal_allowed.then(|| host.clone()),
        })
        .await;
    // Through the server: now that the browser is let in, offer this host's own
    // addresses too. On the same LAN or tailnet it goes direct; elsewhere the relay stays.
    if relayed {
        let mut sockets = Vec::new();
        for ip in rtc::direct_addresses(nexa_net::discovery::local_ips()) {
            if let Ok(s) = tokio::net::UdpSocket::bind(SocketAddr::new(ip, 0)).await {
                sockets.push(s);
            }
        }
        let list: Vec<String> = sockets
            .iter()
            .filter_map(|s| s.local_addr().ok())
            .filter_map(|a| str0m::Candidate::host(a, "udp").ok())
            .map(|c| c.to_sdp_string())
            .collect();
        if !sockets.is_empty() {
            let _ = cmd_tx.send(Cmd::Direct(sockets)).await;
            let _ = send(&mut ws, json!({"type": "candidates", "list": list})).await;
        }
    }
    host.event(WebEvent::SessionStarted {
        id,
        browser,
        remote: peer,
        fingerprint,
        input: input_allowed,
        clipboard: clipboard_allowed,
        terminal: terminal_allowed,
        video: screen.is_some(),
    });

    let mine = Arc::new(tokio::sync::Notify::new());
    shared
        .kicks
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id, mine.clone());
    let _kick = Kick(&shared, id);
    let reason = loop {
        tokio::select! {
            _ = shared.kick.notified() => break "ended by the host".to_string(),
            _ = mine.notified() => break "ended by the host".to_string(),
            ended = &mut driver => break match ended {
                Ok(Ended::Disconnected) => "the browser disconnected".to_string(),
                Ok(Ended::Error(e)) => e,
                Err(_) => "stopped".to_string(),
            },
            m = ws.next_text() => match m {
                Some(t) if matches!(serde_json::from_str(&t), Ok(FromBrowser::Bye)) => {
                    break "the browser left".to_string();
                }
                Some(_) => continue,
                // Its signaling WebSocket: through a server, the server closed it.
                None => break "the browser's signaling connection closed".to_string(),
            },
        }
    };
    driver.abort();
    drop(cmd_tx);
    drop(cursor);
    drop(media);
    let _ = send(&mut ws, json!({"type": "ended", "reason": reason})).await;
    ws.close().await;
    host.event(WebEvent::SessionEnded { id, reason });
}
